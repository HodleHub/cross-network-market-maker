use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use super::error::LightningError;

pub(crate) fn bytes_to_hex(value: &[u8]) -> String {
    hex::encode(value)
}

pub(crate) fn decode_hash_hex(value: &str) -> Result<[u8; 32], LightningError> {
    let bytes = hex::decode(value).map_err(|_| LightningError::PaymentHashInvalid)?;

    bytes
        .try_into()
        .map_err(|_| LightningError::PaymentHashInvalid)
}

pub(crate) fn decode_base64(value: &str) -> Result<Vec<u8>, LightningError> {
    STANDARD
        .decode(value)
        .map_err(|_| LightningError::InvalidResponse("invalid base64 bytes".to_owned()))
}

pub(crate) fn encode_base64(value: &[u8]) -> String {
    STANDARD.encode(value)
}

pub(crate) fn encode_base64_url(value: &[u8]) -> String {
    STANDARD.encode(value).replace('+', "-").replace('/', "_")
}

pub(crate) fn value_as_string(value: Option<&Value>) -> Option<String> {
    value.and_then(|item| match item {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    })
}

pub(crate) fn value_as_u64(value: Option<&Value>) -> Option<u64> {
    value_as_string(value)?.parse::<u64>().ok()
}

pub(crate) fn value_as_bool(value: Option<&Value>) -> Option<bool> {
    value.and_then(Value::as_bool)
}

pub(crate) fn value_as_object(value: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    value.and_then(Value::as_object)
}
