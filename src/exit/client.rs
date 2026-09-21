//! Minimal TLS-pinned LND client for native force-close observation.

use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::{Method, Url, redirect};
use serde_json::Value;

use crate::lightning::{LightningNetwork, LightningTip, LndRestConfig};

use super::error::ExitError;
use super::parser::parse_pending_force_closes;
use super::types::{ChannelPoint, ExitChannel, PendingForceClose, WalletTransactionOutput};

/// Result of requesting a unilateral close, reconciled through pending channels when needed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForceCloseResult {
    /// Channel point requested for force close.
    pub channel_point: ChannelPoint,
    /// Whether LND returned a close response before the request ended.
    pub response_received: bool,
    /// Whether pending force-close state confirmed the side effect after an unknown response.
    pub reconciled_pending: bool,
}

/// TLS-pinned LND client used only for native exit operations.
pub struct ExitLndClient {
    http: Client,
    base_url: Url,
    macaroon_hex: String,
    timeout: Duration,
}

impl ExitLndClient {
    /// Creates a loopback HTTPS client with the supplied runtime certificate and macaroon.
    pub fn new(config: LndRestConfig) -> Result<Self, ExitError> {
        if config.network != LightningNetwork::Regtest {
            return Err(ExitError::WrongNetwork(
                "only Lightning regtest is enabled".to_owned(),
            ));
        }

        let base_url =
            Url::parse(&config.base_url).map_err(|error| ExitError::Request(error.to_string()))?;

        if base_url.scheme() != "https"
            || !is_loopback(base_url.host_str())
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
            || !matches!(base_url.path(), "" | "/")
        {
            return Err(ExitError::WrongNetwork(
                "LND exit endpoint must be HTTPS loopback without a path or credentials".to_owned(),
            ));
        }

        if hex::decode(&config.macaroon_hex)
            .map_err(|_| ExitError::Request("LND macaroon is not hexadecimal".to_owned()))?
            .is_empty()
        {
            return Err(ExitError::Request("LND macaroon is empty".to_owned()));
        }
        if config.request_timeout.is_zero() {
            return Err(ExitError::Request(
                "LND request timeout must be positive".to_owned(),
            ));
        }

        let tls_config = crate::lightning::tls::pinned_client_config(&config.tls_certificate_pem)
            .map_err(ExitError::Request)?;
        let http = Client::builder()
            .use_preconfigured_tls(tls_config)
            .redirect(redirect::Policy::none())
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| ExitError::Request(error.to_string()))?;

        Ok(Self {
            http,
            base_url,
            macaroon_hex: config.macaroon_hex,
            timeout: config.request_timeout,
        })
    }

    /// Qualifies the LND endpoint as synchronized Bitcoin regtest.
    pub fn get_info(&self) -> Result<LightningTip, ExitError> {
        let payload = self.request(Method::GET, "/v1/getinfo")?;
        let chains = payload
            .get("chains")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .ok_or_else(|| ExitError::InvalidData("LND getinfo lacks chains".to_owned()))?;
        let chain = chains
            .get("chain")
            .and_then(Value::as_str)
            .ok_or_else(|| ExitError::InvalidData("LND chain is missing".to_owned()))?;
        let network = chains
            .get("network")
            .and_then(Value::as_str)
            .ok_or_else(|| ExitError::InvalidData("LND network is missing".to_owned()))?;
        let synced = payload
            .get("synced_to_chain")
            .and_then(Value::as_bool)
            .ok_or_else(|| ExitError::InvalidData("LND sync flag is missing".to_owned()))?;
        let block_height = unsigned(&payload, "block_height")?;

        if chain != "bitcoin" || network != "regtest" || !synced {
            return Err(ExitError::WrongNetwork(
                "LND must report synchronized bitcoin regtest".to_owned(),
            ));
        }

        Ok(LightningTip {
            identity_pubkey: required_string(&payload, "identity_pubkey")?.to_owned(),
            alias: required_string(&payload, "alias")?.to_owned(),
            block_height,
            synced_to_chain: synced,
        })
    }

    /// Lists all channels after qualifying the payer LND node.
    pub fn list_channels(&self) -> Result<Vec<ExitChannel>, ExitError> {
        self.get_info()?;
        let payload = self.request(
            Method::GET,
            "/v1/channels?active_only=false&public_only=false",
        )?;

        super::parser::parse_exit_channels(&payload)
    }

    /// Reads pending force closes after qualifying the payer LND node.
    pub fn pending_force_closes(&self) -> Result<Vec<PendingForceClose>, ExitError> {
        self.get_info()?;
        let payload = self.request(Method::GET, "/v1/channels/pending")?;

        parse_pending_force_closes(&payload)
    }

    /// Returns LND wallet output ownership observations without exposing raw transactions.
    pub fn wallet_transaction_outputs(&self) -> Result<Vec<WalletTransactionOutput>, ExitError> {
        self.get_info()?;
        let payload = self.request(Method::GET, "/v1/transactions")?;
        let transactions = payload
            .get("transactions")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                ExitError::InvalidData("LND transactions lacks transactions".to_owned())
            })?;
        let mut outputs = Vec::new();

        for transaction in transactions {
            append_wallet_outputs(transaction, &mut outputs)?;
        }

        Ok(outputs)
    }

    /// Requests an official force close and reconciles a streaming timeout through pending state.
    pub fn force_close(&self, channel_point: ChannelPoint) -> Result<ForceCloseResult, ExitError> {
        self.get_info()?;
        let path = format!(
            "/v1/channels/{}/{}?force=true",
            channel_point.funding_txid, channel_point.funding_vout
        );

        match self.request(Method::DELETE, &path) {
            Ok(_) => Ok(ForceCloseResult {
                channel_point,
                response_received: true,
                reconciled_pending: false,
            }),
            Err(request_error) => {
                let pending = self.pending_force_closes()?;
                let reconciled_pending = pending
                    .iter()
                    .any(|item| item.channel_point == channel_point);

                if reconciled_pending {
                    return Ok(ForceCloseResult {
                        channel_point,
                        response_received: false,
                        reconciled_pending: true,
                    });
                }

                Err(request_error)
            }
        }
    }

    fn request(&self, method: Method, path: &str) -> Result<Value, ExitError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|error| ExitError::Request(error.to_string()))?;
        let response = self
            .http
            .request(method, url)
            .header("Grpc-Metadata-macaroon", &self.macaroon_hex)
            .timeout(self.timeout)
            .send()
            .map_err(|error| ExitError::Request(error.to_string()))?;
        let status = response.status();
        let payload = response
            .json::<Value>()
            .map_err(|error| ExitError::Request(error.to_string()))?;

        if !status.is_success() {
            return Err(ExitError::Request(format!("LND HTTP status {status}")));
        }

        Ok(payload)
    }
}

fn is_loopback(host: Option<&str>) -> bool {
    matches!(host, Some("localhost" | "127.0.0.1" | "::1"))
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, ExitError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ExitError::InvalidData(format!("LND field {field} is missing")))
}

fn unsigned(value: &Value, field: &str) -> Result<u64, ExitError> {
    let raw = value
        .get(field)
        .ok_or_else(|| ExitError::InvalidData(format!("LND field {field} is missing")))?;
    let text = raw
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| raw.to_string());

    text.parse::<u64>()
        .map_err(|_| ExitError::InvalidData(format!("LND field {field} is invalid")))
}

fn append_wallet_outputs(
    transaction: &Value,
    outputs: &mut Vec<WalletTransactionOutput>,
) -> Result<(), ExitError> {
    let txid = required_string(transaction, "tx_hash")?.to_owned();
    let details = transaction
        .get("output_details")
        .and_then(Value::as_array)
        .ok_or_else(|| ExitError::InvalidData("LND transaction outputs are missing".to_owned()))?;

    for detail in details {
        let output_index = unsigned(detail, "output_index")?
            .try_into()
            .map_err(|_| ExitError::InvalidData("LND output index exceeds u32".to_owned()))?;
        let is_our_address = detail
            .get("is_our_address")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let addresses = detail
            .get("addresses")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        for address in addresses {
            outputs.push(WalletTransactionOutput {
                txid: txid.clone(),
                output_index,
                address,
                is_our_address,
            });
        }
    }

    Ok(())
}
