use serde_json::Value;
use sha2::{Digest, Sha256};

use super::encoding::{
    decode_base64, decode_hash_hex, value_as_object, value_as_string, value_as_u64,
};
use super::types::{PaymentObservation, PaymentState};

pub(crate) fn parse_streaming_json(body: &str) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        return Ok(value);
    }

    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .next_back()
        .ok_or_else(|| "no JSON object in LND response".to_owned())
}

pub(crate) fn parse_payment_observation(
    value: &Value,
    payment_hash: [u8; 32],
) -> Option<PaymentObservation> {
    let payload = value_as_object(Some(value))?;
    let nested = value_as_object(payload.get("result")).unwrap_or(payload);
    let raw_status = nested
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN");
    let state = match raw_status {
        "IN_FLIGHT" => PaymentState::InFlight,
        "SUCCEEDED" => PaymentState::Succeeded,
        "FAILED" => PaymentState::Failed,
        _ => PaymentState::Unknown,
    };

    if let Some(raw_hash) = nested.get("payment_hash").and_then(Value::as_str) {
        let returned_hash = match decode_hash_hex(raw_hash) {
            Ok(hash) => hash,
            Err(_) => {
                let bytes = decode_base64(raw_hash).ok()?;

                bytes.try_into().ok()?
            }
        };

        if returned_hash != payment_hash {
            return None;
        }
    }

    let failure_reason = nested
        .get("failure_reason")
        .or_else(|| nested.get("payment_error"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let fee_sats = value_as_u64(nested.get("fee_sat"));
    let raw_preimage = value_as_string(nested.get("payment_preimage"));
    let payment_preimage = raw_preimage.and_then(|text| {
        decode_hash_hex(&text)
            .ok()
            .or_else(|| decode_base64(&text).ok()?.try_into().ok())
    });

    if state == PaymentState::Succeeded {
        let Some(preimage) = payment_preimage else {
            return Some(PaymentObservation {
                payment_hash,
                state: PaymentState::Unknown,
                failure_reason: Some("LND_PAYMENT_PREIMAGE_MISSING".to_owned()),
                fee_sats,
                payment_preimage: None,
            });
        };

        let computed_hash = Sha256::digest(preimage);

        if computed_hash.as_slice() != payment_hash {
            return Some(PaymentObservation {
                payment_hash,
                state: PaymentState::Unknown,
                failure_reason: Some("LND_PAYMENT_PREIMAGE_MISMATCH".to_owned()),
                fee_sats,
                payment_preimage: None,
            });
        }

        return Some(PaymentObservation {
            payment_hash,
            state,
            failure_reason,
            fee_sats,
            payment_preimage: Some(preimage),
        });
    }

    Some(PaymentObservation {
        payment_hash,
        state,
        failure_reason,
        fee_sats,
        payment_preimage: None,
    })
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::parse_payment_observation;
    use crate::lightning::types::PaymentState;

    #[test]
    fn failed_zero_preimage_is_not_exposed() {
        let payment_hash = [0x11_u8; 32];
        let observation = parse_payment_observation(
            &json!({
                "result": {
                    "payment_hash": hex::encode(payment_hash),
                    "payment_preimage": base64::engine::general_purpose::STANDARD.encode([0_u8; 32]),
                    "status": "FAILED"
                }
            }),
            payment_hash,
        );

        assert_eq!(
            observation.as_ref().map(|value| value.state),
            Some(PaymentState::Failed)
        );
        assert_eq!(observation.and_then(|value| value.payment_preimage), None);
    }

    #[test]
    fn succeeded_preimage_is_exposed_only_after_hash_verification() {
        let preimage = [0x22_u8; 32];
        let payment_hash: [u8; 32] = Sha256::digest(preimage).into();
        let observation = parse_payment_observation(
            &json!({
                "payment_hash": hex::encode(payment_hash),
                "payment_preimage": base64::engine::general_purpose::STANDARD.encode(preimage),
                "status": "SUCCEEDED"
            }),
            payment_hash,
        );

        assert_eq!(
            observation.as_ref().map(|value| value.state),
            Some(PaymentState::Succeeded)
        );
        assert_eq!(
            observation.and_then(|value| value.payment_preimage),
            Some(preimage)
        );
    }
}
