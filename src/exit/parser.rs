//! Strict parsing for LND channel and pending force-close JSON.

use std::str::FromStr;

use base64::Engine;
use bitcoin::Txid;
use serde_json::Value;

use super::error::ExitError;
use super::types::{ChannelPoint, ExitChannel, ExitHtlc, PendingForceClose, PendingHtlc};

/// Parses an LND `channel_point` such as `txid:vout`.
pub fn parse_channel_point(value: &str) -> Result<ChannelPoint, ExitError> {
    let (txid, vout) = value
        .rsplit_once(':')
        .ok_or_else(|| ExitError::InvalidData("channel point must contain txid:vout".to_owned()))?;
    Txid::from_str(txid)
        .map_err(|_| ExitError::InvalidData("channel point txid is invalid".to_owned()))?;
    let funding_vout = vout
        .parse::<u32>()
        .map_err(|_| ExitError::InvalidData("channel point vout is invalid".to_owned()))?;

    Ok(ChannelPoint {
        funding_txid: txid.to_owned(),
        funding_vout,
        serialized: value.to_owned(),
    })
}

/// Parses `listchannels`/`pendingchannels` channel records without lossy numbers.
pub fn parse_exit_channels(value: &Value) -> Result<Vec<ExitChannel>, ExitError> {
    let channels = value
        .get("channels")
        .and_then(Value::as_array)
        .ok_or_else(|| ExitError::InvalidData("LND channel response lacks channels".to_owned()))?;

    channels.iter().map(parse_channel).collect()
}

/// Parses LND pending force-close records, preserving negative maturity deltas.
pub fn parse_pending_force_closes(value: &Value) -> Result<Vec<PendingForceClose>, ExitError> {
    let channels = value
        .get("pending_force_closing_channels")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ExitError::InvalidData("LND pending response lacks force closes".to_owned())
        })?;

    channels.iter().map(parse_pending_channel).collect()
}

fn parse_channel(value: &Value) -> Result<ExitChannel, ExitError> {
    let channel_point = parse_channel_point(required_string(value, "channel_point")?)?;
    let pending = value
        .get("pending_htlcs")
        .map(parse_exit_htlc_list)
        .transpose()?
        .unwrap_or_default();

    Ok(ExitChannel {
        channel_point,
        remote_pubkey: required_string(value, "remote_pubkey")?.to_owned(),
        capacity_sats: required_unsigned(value, "capacity")?,
        local_balance_sats: required_unsigned(value, "local_balance")?,
        active: value
            .get("active")
            .and_then(Value::as_bool)
            .ok_or_else(|| ExitError::InvalidData("channel active flag is invalid".to_owned()))?,
        pending_htlcs: pending,
    })
}

fn parse_pending_channel(value: &Value) -> Result<PendingForceClose, ExitError> {
    let channel = value
        .get("channel")
        .ok_or_else(|| ExitError::InvalidData("pending close lacks channel".to_owned()))?;
    let channel_point = parse_channel_point(required_string(channel, "channel_point")?)?;
    let pending = value
        .get("pending_htlcs")
        .ok_or_else(|| ExitError::InvalidData("pending close lacks HTLCs".to_owned()))?;
    let pending_htlcs = parse_pending_htlc_list(pending)?;

    Ok(PendingForceClose {
        channel_point,
        closing_txid: required_string(value, "closing_txid")?.to_owned(),
        limbo_balance_sats: required_unsigned(value, "limbo_balance")?,
        maturity_height: required_signed(value, "maturity_height")?,
        blocks_til_maturity: required_signed(value, "blocks_til_maturity")?,
        pending_htlcs,
    })
}

fn parse_exit_htlc_list(value: &Value) -> Result<Vec<ExitHtlc>, ExitError> {
    let entries = value
        .as_array()
        .ok_or_else(|| ExitError::InvalidData("channel HTLC list is invalid".to_owned()))?;

    entries.iter().map(parse_htlc).collect()
}

fn parse_pending_htlc_list(value: &Value) -> Result<Vec<PendingHtlc>, ExitError> {
    let entries = value
        .as_array()
        .ok_or_else(|| ExitError::InvalidData("pending HTLC list is invalid".to_owned()))?;

    entries.iter().map(parse_pending_htlc).collect()
}

fn parse_pending_htlc(value: &Value) -> Result<PendingHtlc, ExitError> {
    Ok(PendingHtlc {
        outpoint: parse_channel_point(required_string(value, "outpoint")?)?,
        amount_sats: required_unsigned(value, "amount")?,
        incoming: value
            .get("incoming")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                ExitError::InvalidData("pending HTLC incoming flag is invalid".to_owned())
            })?,
        maturity_height: required_signed(value, "maturity_height")?,
        blocks_til_maturity: required_signed(value, "blocks_til_maturity")?,
        stage: required_unsigned(value, "stage")?
            .try_into()
            .map_err(|_| ExitError::InvalidData("pending HTLC stage exceeds u32".to_owned()))?,
    })
}

fn parse_htlc(value: &Value) -> Result<ExitHtlc, ExitError> {
    Ok(ExitHtlc {
        hash_lock: parse_hash_lock(required_string(value, "hash_lock")?)?,
        amount_sats: required_unsigned(value, "amount")?,
        incoming: value
            .get("incoming")
            .and_then(Value::as_bool)
            .ok_or_else(|| ExitError::InvalidData("HTLC incoming flag is invalid".to_owned()))?,
        expiration_height: required_unsigned(value, "expiration_height")?,
    })
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, ExitError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ExitError::InvalidData(format!("LND field {field} is invalid")))
}

fn required_unsigned(value: &Value, field: &str) -> Result<u64, ExitError> {
    let raw = value
        .get(field)
        .ok_or_else(|| ExitError::InvalidData(format!("LND field {field} is missing")))?;

    let text = raw
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| raw.to_string());

    if text.is_empty()
        || text.starts_with('-')
        || !text.chars().all(|character| character.is_ascii_digit())
    {
        return Err(ExitError::InvalidData(format!(
            "LND field {field} is not an unsigned integer"
        )));
    }

    text.parse::<u64>()
        .map_err(|_| ExitError::InvalidData(format!("LND field {field} exceeds u64")))
}

fn required_signed(value: &Value, field: &str) -> Result<i64, ExitError> {
    let raw = value
        .get(field)
        .ok_or_else(|| ExitError::InvalidData(format!("LND field {field} is missing")))?;
    let text = raw
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| raw.to_string());

    if text.is_empty()
        || !text
            .strip_prefix('-')
            .unwrap_or(&text)
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return Err(ExitError::InvalidData(format!(
            "LND field {field} is not a signed integer"
        )));
    }

    text.parse::<i64>()
        .map_err(|_| ExitError::InvalidData(format!("LND field {field} exceeds i64")))
}

fn parse_hash_lock(value: &str) -> Result<[u8; 32], ExitError> {
    let bytes = if value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit())
    {
        hex::decode(value)
            .map_err(|_| ExitError::InvalidData("HTLC hash lock is invalid hex".to_owned()))?
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(value)
            .map_err(|_| ExitError::InvalidData("HTLC hash lock is invalid base64".to_owned()))?
    };

    bytes
        .try_into()
        .map_err(|_| ExitError::InvalidData("HTLC hash lock must contain 32 bytes".to_owned()))
}
