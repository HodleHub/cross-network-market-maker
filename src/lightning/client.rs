use std::error::Error as StdError;
use std::time::{Duration, SystemTime};

use reqwest::blocking::{Client, RequestBuilder};
use reqwest::{Method, Url};
use serde_json::{Map, Value, json};
use sha2::Digest;

use super::bolt11::validate_payment_request;
use super::encoding::{
    bytes_to_hex, decode_base64, encode_base64, encode_base64_url, value_as_bool, value_as_object,
    value_as_string, value_as_u64,
};
use super::error::LightningError;
use super::observation::{parse_payment_observation, parse_streaming_json};
use super::types::{
    CancelHoldRequest, CancellationReceipt, HoldInvoice, HoldInvoiceRequest, InvoiceState,
    LightningNetwork, LightningTip, LndRestConfig, PaymentObservation, PaymentRequest,
    PaymentState, SettlementReceipt,
};

const MAX_CLTV_DELTA: u32 = 2_016;
const MAX_FEE_LIMIT_SATS: u64 = 100_000;
const MAX_PAYMENT_TIMEOUT: Duration = Duration::from_secs(120);

/// Blocking, TLS-pinned LND REST adapter for the local regtest profile.
pub struct LndRestClient {
    http: Client,
    base_url: Url,
    macaroon_hex: String,
    default_timeout: Duration,
}

impl LndRestClient {
    /// Build a client that trusts only the supplied LND certificate.
    pub fn new(config: LndRestConfig) -> Result<Self, LightningError> {
        if config.network != LightningNetwork::Regtest {
            return Err(LightningError::InvalidConfiguration(
                "only regtest is enabled".to_owned(),
            ));
        }

        let base_url = Url::parse(&config.base_url)
            .map_err(|error| LightningError::InvalidConfiguration(error.to_string()))?;

        if base_url.scheme() != "https" || !is_loopback_host(base_url.host_str()) {
            return Err(LightningError::EndpointNotLocalTls);
        }

        if !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
            || !matches!(base_url.path(), "" | "/")
        {
            return Err(LightningError::InvalidConfiguration(
                "LND endpoint must not contain credentials, query, fragment, or path".to_owned(),
            ));
        }

        if hex::decode(&config.macaroon_hex)
            .map_err(|_| LightningError::InvalidMacaroon)?
            .is_empty()
        {
            return Err(LightningError::InvalidMacaroon);
        }

        if config.request_timeout.is_zero() {
            return Err(LightningError::InvalidConfiguration(
                "request timeout must be positive".to_owned(),
            ));
        }

        let tls_config = super::tls::pinned_client_config(&config.tls_certificate_pem)
            .map_err(LightningError::InvalidCertificate)?;
        let http = Client::builder()
            .use_preconfigured_tls(tls_config)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| LightningError::InvalidConfiguration(error.to_string()))?;

        Ok(Self {
            http,
            base_url,
            macaroon_hex: config.macaroon_hex,
            default_timeout: config.request_timeout,
        })
    }

    /// Query and qualify the LND node before any side effect.
    pub fn get_info(&self) -> Result<LightningTip, LightningError> {
        let response = self.request(Method::GET, "/v1/getinfo", None, self.default_timeout)?;
        let payload = object(&response)?;
        let chains = payload
            .get("chains")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_object)
            .ok_or_else(|| LightningError::InvalidResponse("chains is missing".to_owned()))?;
        let chain = chains
            .get("chain")
            .and_then(Value::as_str)
            .ok_or_else(|| LightningError::InvalidResponse("chain is missing".to_owned()))?;
        let network = chains
            .get("network")
            .and_then(Value::as_str)
            .ok_or_else(|| LightningError::InvalidResponse("network is missing".to_owned()))?;
        let synced = value_as_bool(payload.get("synced_to_chain")).ok_or_else(|| {
            LightningError::InvalidResponse("synced_to_chain is missing".to_owned())
        })?;
        let block_height = value_as_u64(payload.get("block_height"))
            .ok_or_else(|| LightningError::InvalidResponse("block_height is missing".to_owned()))?;
        let identity_pubkey = payload
            .get("identity_pubkey")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                LightningError::InvalidResponse("identity_pubkey is missing".to_owned())
            })?;
        let alias = payload
            .get("alias")
            .and_then(Value::as_str)
            .ok_or_else(|| LightningError::InvalidResponse("alias is missing".to_owned()))?;

        if chain != "bitcoin" {
            return Err(LightningError::WrongChain);
        }

        if network != "regtest" {
            return Err(LightningError::WrongNetwork);
        }

        if !synced {
            return Err(LightningError::NotSynced);
        }

        Ok(LightningTip {
            identity_pubkey: identity_pubkey.to_owned(),
            alias: alias.to_owned(),
            block_height,
            synced_to_chain: synced,
        })
    }

    /// Create a private hold invoice after qualifying the node.
    pub fn create_hold_invoice(
        &self,
        request: HoldInvoiceRequest,
    ) -> Result<HoldInvoice, LightningError> {
        self.get_info()?;

        if request.amount_sats == 0
            || request.cltv_expiry == 0
            || request.cltv_expiry > MAX_CLTV_DELTA
        {
            return Err(LightningError::ParameterOutOfRange);
        }

        let mut body = Map::new();
        body.insert(
            "hash".to_owned(),
            Value::String(encode_base64(&request.payment_hash)),
        );
        body.insert("value".to_owned(), Value::from(request.amount_sats));
        body.insert("cltv_expiry".to_owned(), Value::from(request.cltv_expiry));
        body.insert("private".to_owned(), Value::Bool(true));

        if let Some(memo) = request.memo {
            body.insert("memo".to_owned(), Value::String(memo));
        }

        let response = self.request(
            Method::POST,
            "/v2/invoices/hodl",
            Some(Value::Object(body)),
            self.default_timeout,
        )?;
        let payload = object(&response)?;
        let payment_request = payload
            .get("payment_request")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                LightningError::InvalidResponse("payment_request is missing".to_owned())
            })?;
        let add_index = value_as_string(payload.get("add_index"))
            .ok_or_else(|| LightningError::InvalidResponse("add_index is missing".to_owned()))?;

        if let Some(raw_hash) = payload.get("r_hash").and_then(Value::as_str) {
            let returned_hash = decode_base64(raw_hash)?;

            if returned_hash.as_slice() != request.payment_hash {
                return Err(LightningError::InvoiceHashMismatch);
            }
        }

        Ok(HoldInvoice {
            payment_hash: request.payment_hash,
            payment_request: Some(payment_request),
            state: InvoiceState::Open,
            amount_sats: request.amount_sats,
            cltv_expiry: request.cltv_expiry,
            add_index,
            accepted_amount_sats: None,
            accepted_expiry_height: None,
        })
    }

    /// Query a hold invoice and summarize its accepted HTLCs.
    pub fn lookup_invoice(&self, payment_hash: [u8; 32]) -> Result<HoldInvoice, LightningError> {
        self.get_info()?;
        let path = format!("/v1/invoice/{}", bytes_to_hex(&payment_hash));
        let response = self.request(Method::GET, &path, None, self.default_timeout)?;
        let payload = object(&response)?;

        if let Some(raw_hash) = payload.get("r_hash").and_then(Value::as_str) {
            let returned_hash = decode_base64(raw_hash)?;

            if returned_hash.as_slice() != payment_hash {
                return Err(LightningError::InvoiceHashMismatch);
            }
        }

        let amount_sats = value_as_u64(payload.get("value")).ok_or_else(|| {
            LightningError::InvalidResponse("invoice value is missing".to_owned())
        })?;
        let cltv_expiry = value_as_u64(payload.get("cltv_expiry"))
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                LightningError::InvalidResponse("invoice cltv_expiry is missing".to_owned())
            })?;
        let add_index = value_as_string(payload.get("add_index")).ok_or_else(|| {
            LightningError::InvalidResponse("invoice add_index is missing".to_owned())
        })?;
        let state = parse_invoice_state(payload.get("state").and_then(Value::as_str));
        let payment_request = payload
            .get("payment_request")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let (accepted_amount_sats, accepted_expiry_height) =
            summarize_accepted_htlcs(payload.get("htlcs"));

        Ok(HoldInvoice {
            payment_hash,
            payment_request,
            state,
            amount_sats,
            cltv_expiry,
            add_index,
            accepted_amount_sats,
            accepted_expiry_height,
        })
    }

    /// Cancel an accepted hold before the caller's deadline.
    pub fn cancel_hold(
        &self,
        request: CancelHoldRequest,
    ) -> Result<CancellationReceipt, LightningError> {
        self.get_info()?;
        let invoice = self.lookup_invoice(request.payment_hash)?;

        if invoice.state == InvoiceState::Canceled {
            return Ok(CancellationReceipt {
                payment_hash: request.payment_hash,
            });
        }

        if invoice.state != InvoiceState::Accepted {
            return Err(LightningError::UnsafeCancelState);
        }

        if request
            .deadline
            .is_some_and(|deadline| SystemTime::now() >= deadline)
        {
            return Err(LightningError::CancelDeadlinePassed);
        }

        self.request(
            Method::POST,
            "/v2/invoices/cancel",
            Some(json!({ "payment_hash": encode_base64(&request.payment_hash) })),
            self.default_timeout,
        )?;

        Ok(CancellationReceipt {
            payment_hash: request.payment_hash,
        })
    }

    /// Settle an accepted hold with a hash-checked preimage.
    pub fn settle_hold(
        &self,
        payment_hash: [u8; 32],
        preimage: [u8; 32],
    ) -> Result<SettlementReceipt, LightningError> {
        self.get_info()?;

        if sha2::Sha256::digest(preimage).as_slice() != payment_hash {
            return Err(LightningError::PreimageMismatch);
        }

        let invoice = self.lookup_invoice(payment_hash)?;

        if invoice.state == InvoiceState::Settled {
            return Ok(SettlementReceipt { payment_hash });
        }

        if invoice.state != InvoiceState::Accepted {
            return Err(LightningError::UnsafeSettleState);
        }

        self.request(
            Method::POST,
            "/v2/invoices/settle",
            Some(json!({ "preimage": encode_base64(&preimage) })),
            self.default_timeout,
        )?;

        let settled = self.lookup_invoice(payment_hash)?;

        if settled.state != InvoiceState::Settled {
            return Err(LightningError::InvalidResponse(
                "LND hold did not reach SETTLED".to_owned(),
            ));
        }

        Ok(SettlementReceipt { payment_hash })
    }

    /// Pay a hold invoice after local BOLT11 binding validation.
    pub fn pay(&self, request: PaymentRequest) -> Result<PaymentObservation, LightningError> {
        validate_payment_request(
            &request.payment_request,
            request.payment_hash,
            request.expected_amount_sats,
        )?;
        self.get_info()?;

        if request.expected_amount_sats == 0
            || request.fee_limit_sats > MAX_FEE_LIMIT_SATS
            || request.cltv_limit == 0
            || request.cltv_limit > MAX_CLTV_DELTA
            || request.timeout.is_zero()
            || request.timeout > MAX_PAYMENT_TIMEOUT
        {
            return Err(LightningError::ParameterOutOfRange);
        }

        let timeout_seconds = request.timeout.as_secs().max(1);
        let body = json!({
            "payment_request": request.payment_request,
            "fee_limit_sat": request.fee_limit_sats,
            "cltv_limit": request.cltv_limit,
            "timeout_seconds": timeout_seconds,
        });
        let response =
            self.request_result(Method::POST, "/v2/router/send", Some(body), request.timeout);

        let value = match response {
            Ok(value) => value,
            Err(LightningError::RequestTimeout) | Err(LightningError::Transport(_)) => {
                return Ok(unknown_payment(request.payment_hash));
            }
            Err(error) => return Err(error),
        };

        Ok(parse_payment_observation(&value, request.payment_hash)
            .unwrap_or_else(|| unknown_payment(request.payment_hash)))
    }

    /// Track a payment by hash; transport uncertainty remains UNKNOWN.
    pub fn track_by_hash(
        &self,
        payment_hash: [u8; 32],
        timeout: Duration,
    ) -> Result<PaymentObservation, LightningError> {
        self.get_info()?;

        if timeout.is_zero() || timeout > MAX_PAYMENT_TIMEOUT {
            return Err(LightningError::ParameterOutOfRange);
        }

        let path = format!("/v2/router/track/{}", encode_base64_url(&payment_hash));
        let response = match self.request_result(Method::GET, &path, None, timeout) {
            Ok(value) => value,
            Err(LightningError::RequestTimeout) | Err(LightningError::Transport(_)) => {
                return Ok(unknown_payment(payment_hash));
            }
            Err(error) => return Err(error),
        };

        Ok(parse_payment_observation(&response, payment_hash)
            .unwrap_or_else(|| unknown_payment(payment_hash)))
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, LightningError> {
        self.request_result(method, path, body, timeout)
    }

    fn request_result(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, LightningError> {
        let url = self
            .base_url
            .join(path.trim_start_matches('/'))
            .map_err(|error| LightningError::InvalidConfiguration(error.to_string()))?;
        let mut request = self
            .http
            .request(method, url)
            .timeout(timeout)
            .header("Accept", "application/json")
            .header("Grpc-Metadata-macaroon", &self.macaroon_hex);

        if let Some(value) = body {
            request = request.json(&value);
        }

        let response = send(request)?;
        let status = response.status();
        let text = response
            .text()
            .map_err(|error| LightningError::Transport(error.to_string()))?;
        let value = parse_streaming_json(&text).map_err(LightningError::InvalidJson)?;

        if !status.is_success() {
            return Err(LightningError::Http {
                status: status.as_u16(),
                message: response_message(&value),
            });
        }

        Ok(value)
    }
}

impl super::types::LightningAdapter for LndRestClient {
    fn current_tip(&self) -> Result<LightningTip, LightningError> {
        self.get_info()
    }

    fn create_hold_invoice(
        &self,
        request: HoldInvoiceRequest,
    ) -> Result<HoldInvoice, LightningError> {
        self.create_hold_invoice(request)
    }

    fn observe_accepted(
        &self,
        payment_hash: [u8; 32],
        expected_amount_sats: u64,
    ) -> Result<HoldInvoice, LightningError> {
        let invoice = self.lookup_invoice(payment_hash)?;

        if invoice.state != InvoiceState::Accepted && invoice.state != InvoiceState::Settled {
            return Err(LightningError::UnsafeSettleState);
        }

        if invoice.accepted_amount_sats != Some(expected_amount_sats)
            || invoice.accepted_expiry_height.is_none()
        {
            return Err(LightningError::InvalidResponse(
                "accepted HTLC amount or expiry is missing".to_owned(),
            ));
        }

        Ok(invoice)
    }

    fn lookup_invoice(&self, payment_hash: [u8; 32]) -> Result<HoldInvoice, LightningError> {
        self.lookup_invoice(payment_hash)
    }

    fn pay(&self, request: PaymentRequest) -> Result<PaymentObservation, LightningError> {
        self.pay(request)
    }

    fn track_by_hash(
        &self,
        payment_hash: [u8; 32],
        timeout: Duration,
    ) -> Result<PaymentObservation, LightningError> {
        self.track_by_hash(payment_hash, timeout)
    }

    fn settle_hold(
        &self,
        payment_hash: [u8; 32],
        preimage: [u8; 32],
    ) -> Result<SettlementReceipt, LightningError> {
        self.settle_hold(payment_hash, preimage)
    }

    fn cancel_hold(
        &self,
        request: CancelHoldRequest,
    ) -> Result<CancellationReceipt, LightningError> {
        self.cancel_hold(request)
    }
}

fn is_loopback_host(host: Option<&str>) -> bool {
    matches!(host, Some("127.0.0.1" | "localhost" | "::1"))
}

fn object(value: &Value) -> Result<&Map<String, Value>, LightningError> {
    value_as_object(Some(value))
        .ok_or_else(|| LightningError::InvalidResponse("expected JSON object".to_owned()))
}

fn parse_invoice_state(value: Option<&str>) -> InvoiceState {
    match value {
        Some("OPEN") => InvoiceState::Open,
        Some("ACCEPTED") => InvoiceState::Accepted,
        Some("SETTLED") => InvoiceState::Settled,
        Some("CANCELED") => InvoiceState::Canceled,
        _ => InvoiceState::Unknown,
    }
}

fn summarize_accepted_htlcs(value: Option<&Value>) -> (Option<u64>, Option<u64>) {
    let Some(items) = value.and_then(Value::as_array) else {
        return (None, None);
    };

    let accepted: Vec<(u64, u64)> = items
        .iter()
        .filter_map(|item| {
            let object = item.as_object()?;
            let state = object.get("state").and_then(Value::as_str)?;

            if state != "ACCEPTED" && state != "SETTLED" && state != "CANCELED" {
                return None;
            }

            let amount_msat = value_as_u64(object.get("amt_msat"))?;
            let expiry_height = value_as_u64(object.get("expiry_height"))?;

            Some((amount_msat, expiry_height))
        })
        .collect();

    if accepted.is_empty()
        || accepted
            .iter()
            .any(|(amount_msat, _)| amount_msat % 1_000 != 0)
    {
        return (None, None);
    }

    let Some(amount_msat) = accepted.iter().try_fold(0_u64, |total, (amount_msat, _)| {
        total.checked_add(*amount_msat)
    }) else {
        return (None, None);
    };
    let expiry_height = accepted.iter().map(|(_, height)| *height).min();

    (Some(amount_msat / 1_000), expiry_height)
}

fn send(request: RequestBuilder) -> Result<reqwest::blocking::Response, LightningError> {
    request.send().map_err(|error| {
        if error.is_timeout() {
            LightningError::RequestTimeout
        } else {
            LightningError::Transport(transport_message(&error))
        }
    })
}

fn transport_message(error: &reqwest::Error) -> String {
    let mut messages = vec![error.to_string()];
    let mut source = error.source();

    while let Some(cause) = source {
        messages.push(cause.to_string());
        source = cause.source();
    }

    messages.join(": ")
}

fn response_message(value: &Value) -> String {
    let Some(object) = value.as_object() else {
        return "LND request failed".to_owned();
    };

    object
        .get("message")
        .or_else(|| object.get("error"))
        .or_else(|| object.get("details"))
        .and_then(Value::as_str)
        .unwrap_or("LND request failed")
        .chars()
        .take(300)
        .collect()
}

fn unknown_payment(payment_hash: [u8; 32]) -> PaymentObservation {
    PaymentObservation {
        payment_hash,
        state: PaymentState::Unknown,
        failure_reason: None,
        fee_sats: None,
        payment_preimage: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::summarize_accepted_htlcs;

    #[test]
    fn canceled_htlc_preserves_accepted_amount_and_expiry() {
        let htlcs = json!([
            { "state": "CANCELED", "amt_msat": 1_000_000, "expiry_height": 700 }
        ]);

        assert_eq!(
            summarize_accepted_htlcs(Some(&htlcs)),
            (Some(1_000), Some(700))
        );
    }

    #[test]
    fn overflowing_htlc_amount_is_rejected() {
        let htlcs = json!([
            { "state": "ACCEPTED", "amt_msat": u64::MAX, "expiry_height": 700 },
            { "state": "SETTLED", "amt_msat": 1_000, "expiry_height": 701 }
        ]);

        assert_eq!(summarize_accepted_htlcs(Some(&htlcs)), (None, None));
    }
}
