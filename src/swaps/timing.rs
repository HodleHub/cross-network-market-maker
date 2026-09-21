use super::error::SwapError;

/// Bitcoin regtest's conservative wall-clock assumption.
pub const BITCOIN_BLOCK_SECONDS: u64 = 600;
/// Elements regtest's conservative wall-clock assumption.
pub const LIQUID_BLOCK_SECONDS: u64 = 60;
/// Claim and observation margin required after the remote timeout.
pub const CLAIM_MARGIN_SECONDS: u64 = 7_200;

/// Timing evidence returned by a successful cross-network inequality check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimingEvidence {
    /// Liquid refund blocks required by the policy.
    pub required_liquid_refund_delta: u64,
    /// Available source-side wall-clock seconds.
    pub available_seconds: u64,
    /// Required destination-side wall-clock seconds.
    pub required_seconds: u64,
}

/// Validate chain-to-Lightning refund headroom before initiating the payment.
pub fn validate_forward_timing(
    lightning_cltv_limit: u64,
    liquid_refund_delta: u64,
    claim_margin_seconds: Option<u64>,
) -> Result<TimingEvidence, SwapError> {
    if lightning_cltv_limit == 0 || liquid_refund_delta == 0 {
        return Err(SwapError::Timing(
            "forward timeout values must be positive".to_owned(),
        ));
    }

    let margin = claim_margin_seconds.unwrap_or(CLAIM_MARGIN_SECONDS);
    let required_seconds = lightning_cltv_limit
        .checked_mul(BITCOIN_BLOCK_SECONDS)
        .and_then(|value| value.checked_add(margin))
        .ok_or_else(|| SwapError::Timing("forward timeout overflows".to_owned()))?;
    let available_seconds = liquid_refund_delta
        .checked_mul(LIQUID_BLOCK_SECONDS)
        .ok_or_else(|| SwapError::Timing("forward refund height overflows".to_owned()))?;
    let required_liquid_refund_delta = required_seconds / LIQUID_BLOCK_SECONDS + 1;

    if available_seconds <= required_seconds {
        return Err(SwapError::Timing(format!(
            "forward refund requires at least {required_liquid_refund_delta} Liquid blocks"
        )));
    }

    Ok(TimingEvidence {
        required_liquid_refund_delta,
        available_seconds,
        required_seconds,
    })
}

/// Validate Lightning-to-chain headroom from actual accepted HTLC expiry.
pub fn validate_reverse_timing(
    current_bitcoin_height: u64,
    accepted_bitcoin_expiry_height: u64,
    liquid_refund_delta: u64,
    claim_margin_seconds: Option<u64>,
) -> Result<TimingEvidence, SwapError> {
    if accepted_bitcoin_expiry_height <= current_bitcoin_height || liquid_refund_delta == 0 {
        return Err(SwapError::Timing(
            "reverse timeout values are invalid".to_owned(),
        ));
    }

    let margin = claim_margin_seconds.unwrap_or(CLAIM_MARGIN_SECONDS);
    let available_bitcoin_blocks = accepted_bitcoin_expiry_height - current_bitcoin_height;
    let available_seconds = available_bitcoin_blocks
        .checked_mul(BITCOIN_BLOCK_SECONDS)
        .ok_or_else(|| SwapError::Timing("Lightning expiry overflows".to_owned()))?;
    let required_seconds = liquid_refund_delta
        .checked_mul(LIQUID_BLOCK_SECONDS)
        .and_then(|value| value.checked_add(margin))
        .ok_or_else(|| SwapError::Timing("reverse refund height overflows".to_owned()))?;

    if available_seconds <= required_seconds {
        return Err(SwapError::Timing(format!(
            "reverse refund leaves only {available_bitcoin_blocks} Bitcoin blocks"
        )));
    }

    Ok(TimingEvidence {
        required_liquid_refund_delta: liquid_refund_delta,
        available_seconds,
        required_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::{validate_forward_timing, validate_reverse_timing};

    #[test]
    fn forward_rejects_the_old_unsafe_delta() {
        assert!(validate_forward_timing(80, 120, None).is_err());
        assert!(validate_forward_timing(80, 1_000, None).is_ok());
    }

    #[test]
    fn reverse_uses_wall_clock_and_not_cross_chain_heights() {
        assert!(validate_reverse_timing(200, 214, 30, None).is_err());
        assert!(validate_reverse_timing(200, 215, 30, None).is_err());
        assert!(validate_reverse_timing(200, 216, 30, None).is_ok());
    }
}
