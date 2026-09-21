//! Small authenticated JSON-RPC client used by the regtest adapters.

use std::time::Duration;

use base64::Engine;
use reqwest::blocking::Client;
use reqwest::{Url, redirect};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;

/// Errors returned by the authenticated JSON-RPC boundary.
#[derive(Debug, Error)]
pub enum RpcError {
    /// The request could not be sent or decoded at the HTTP boundary.
    #[error("rpc transport error for {method}: {message}")]
    Transport { method: String, message: String },
    /// The node returned a JSON-RPC error.
    #[error("rpc error for {method} ({code}): {message}")]
    Remote {
        method: String,
        code: i64,
        message: String,
    },
    /// The node returned a JSON value that did not match the requested type.
    #[error("rpc response decode error for {method}: {message}")]
    Decode { method: String, message: String },
}

/// JSON-RPC response envelope used by Bitcoin Core and Elements Core.
#[derive(Debug, Deserialize)]
struct RpcEnvelope {
    result: Value,
    error: Option<RpcEnvelopeError>,
}

#[derive(Debug, Deserialize)]
struct RpcEnvelopeError {
    code: i64,
    message: String,
}

/// Authenticated blocking JSON-RPC client.
#[derive(Clone)]
pub struct RpcClient {
    client: Client,
    endpoint: String,
    authorization: String,
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RpcClient")
            .field("endpoint", &self.endpoint)
            .field("authorization", &"<redacted>")
            .finish()
    }
}

impl RpcClient {
    /// Creates a client for one local regtest RPC endpoint.
    pub fn new(
        endpoint: impl Into<String>,
        username: impl AsRef<str>,
        password: impl AsRef<str>,
        timeout: Duration,
    ) -> Result<Self, RpcError> {
        let endpoint = endpoint.into();
        validate_endpoint(&endpoint)?;
        let client = Client::builder()
            .timeout(timeout)
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|error| RpcError::Transport {
                method: "client.build".to_owned(),
                message: error.to_string(),
            })?;
        let credentials = format!("{}:{}", username.as_ref(), password.as_ref());
        let authorization = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes())
        );

        Ok(Self {
            client,
            endpoint,
            authorization,
        })
    }

    /// Calls one JSON-RPC method and deserializes its result into `T`.
    pub fn call<T: DeserializeOwned>(&self, method: &str, params: &[Value]) -> Result<T, RpcError> {
        let response = self
            .client
            .post(&self.endpoint)
            .header("authorization", &self.authorization)
            .json(&json!({
                "jsonrpc": "1.0",
                "id": method,
                "method": method,
                "params": params,
            }))
            .send()
            .map_err(|error| RpcError::Transport {
                method: method.to_owned(),
                message: error.to_string(),
            })?;
        let envelope = response
            .json::<RpcEnvelope>()
            .map_err(|error| RpcError::Decode {
                method: method.to_owned(),
                message: error.to_string(),
            })?;

        if let Some(error) = envelope.error {
            return Err(RpcError::Remote {
                method: method.to_owned(),
                code: error.code,
                message: error.message,
            });
        }

        serde_json::from_value(envelope.result).map_err(|error| RpcError::Decode {
            method: method.to_owned(),
            message: error.to_string(),
        })
    }

    /// Calls a method when the result is intentionally ignored.
    pub fn call_unit(&self, method: &str, params: &[Value]) -> Result<(), RpcError> {
        self.call::<Value>(method, params).map(|_| ())
    }

    /// Returns a clone scoped to one wallet RPC path.
    pub fn scoped_to_wallet(&self, wallet: &str) -> Result<Self, RpcError> {
        if wallet.is_empty()
            || !wallet.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
        {
            return Err(RpcError::Transport {
                method: "client.wallet".to_owned(),
                message: "wallet name contains an unsafe path character".to_owned(),
            });
        }

        let mut endpoint = Url::parse(&self.endpoint).map_err(|error| RpcError::Transport {
            method: "client.wallet".to_owned(),
            message: error.to_string(),
        })?;
        let existing_segments = endpoint
            .path_segments()
            .map(|segments| segments.collect::<Vec<_>>())
            .unwrap_or_default();

        if existing_segments.contains(&"wallet") {
            return Err(RpcError::Transport {
                method: "client.wallet".to_owned(),
                message: "RPC client is already scoped to a wallet".to_owned(),
            });
        }

        endpoint
            .path_segments_mut()
            .map_err(|_| RpcError::Transport {
                method: "client.wallet".to_owned(),
                message: "RPC endpoint cannot be scoped to a wallet".to_owned(),
            })?
            .push("wallet")
            .push(wallet);
        let mut scoped = self.clone();
        scoped.endpoint = endpoint.to_string();

        Ok(scoped)
    }
}

fn validate_endpoint(endpoint: &str) -> Result<(), RpcError> {
    let parsed = Url::parse(endpoint).map_err(|error| RpcError::Transport {
        method: "client.build".to_owned(),
        message: format!("invalid RPC endpoint: {error}"),
    })?;

    let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1"));

    if !matches!(parsed.scheme(), "http" | "https") || !loopback {
        return Err(RpcError::Transport {
            method: "client.build".to_owned(),
            message: "RPC endpoint must be an HTTP(S) loopback URL".to_owned(),
        });
    }

    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(RpcError::Transport {
            method: "client.build".to_owned(),
            message: "RPC endpoint must not contain a query or fragment".to_owned(),
        });
    }

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(RpcError::Transport {
            method: "client.build".to_owned(),
            message: "RPC endpoint must not embed credentials".to_owned(),
        });
    }

    Ok(())
}
