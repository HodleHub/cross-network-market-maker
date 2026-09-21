use std::fmt::{Debug, Formatter};

use serde::{Deserialize, Serialize};

use super::error::{CoreError, CoreResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SwapState {
    Created,
    InputFunding,
    InputLocked,
    OutputPending,
    OutputUnknown,
    OutputSettled,
    ClaimPending,
    Settled,
    Refunding,
    Refunded,
    Failed,
}

impl SwapState {
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Created, Self::InputFunding)
                | (Self::InputFunding, Self::InputLocked)
                | (Self::InputFunding, Self::Failed)
                | (Self::InputLocked, Self::OutputPending)
                | (Self::InputLocked, Self::Refunding)
                | (Self::OutputPending, Self::OutputUnknown)
                | (Self::OutputPending, Self::OutputSettled)
                | (Self::OutputUnknown, Self::OutputSettled)
                | (Self::OutputUnknown, Self::Refunding)
                | (Self::OutputSettled, Self::ClaimPending)
                | (Self::ClaimPending, Self::Settled)
                | (Self::Refunding, Self::Refunded)
                | (Self::Refunding, Self::Failed)
        )
    }
}

pub fn validate_transition(current: SwapState, next: SwapState) -> CoreResult<()> {
    if current.can_transition_to(next) {
        return Ok(());
    }

    Err(CoreError::InvalidState(format!(
        "{current:?} cannot transition to {next:?}"
    )))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FundingLockProof {
    Chain {
        txid: String,
        vout: u32,
        amount: String,
        asset_hash: Option<String>,
        hash_commitment: String,
        confirmations: u32,
    },
    Lightning {
        payment_hash: String,
        amount_msat: String,
        expiry_height: u64,
        headroom_blocks: u64,
        observed_state: String,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub enum PaymentEvidence {
    Lightning {
        payment_hash: String,
        amount_msat: String,
        preimage_hex: String,
        observed_state: String,
    },
    Chain {
        txid: String,
        outpoint: String,
        amount: String,
        asset_hash: Option<String>,
        hash_commitment: String,
        confirmations: u32,
        script_verified: bool,
    },
}

impl Debug for PaymentEvidence {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lightning {
                payment_hash,
                amount_msat,
                observed_state,
                ..
            } => formatter
                .debug_struct("PaymentEvidence::Lightning")
                .field("payment_hash", payment_hash)
                .field("amount_msat", amount_msat)
                .field("observed_state", observed_state)
                .field("preimage", &"[redacted]")
                .finish(),
            Self::Chain {
                txid,
                outpoint,
                amount,
                asset_hash,
                hash_commitment,
                confirmations,
                script_verified,
            } => formatter
                .debug_struct("PaymentEvidence::Chain")
                .field("txid", txid)
                .field("outpoint", outpoint)
                .field("amount", amount)
                .field("asset_hash", asset_hash)
                .field("hash_commitment", hash_commitment)
                .field("confirmations", confirmations)
                .field("script_verified", script_verified)
                .finish(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PublicPaymentEvidence {
    Lightning {
        payment_hash: String,
        amount_msat: String,
        observed_state: String,
        preimage_verified: bool,
    },
    Chain {
        txid: String,
        outpoint: String,
        amount: String,
        asset_hash: Option<String>,
        hash_commitment: String,
        confirmations: u32,
        script_verified: bool,
    },
}

impl PaymentEvidence {
    pub fn validate_against(&self, expected_hash: &str, expected_amount: &str) -> CoreResult<()> {
        match self {
            Self::Lightning {
                payment_hash,
                amount_msat,
                preimage_hex,
                observed_state,
            } => {
                if payment_hash != expected_hash
                    || amount_msat != expected_amount
                    || observed_state != "SUCCEEDED"
                {
                    return Err(CoreError::Adapter(
                        "Lightning evidence does not bind hash, amount, and state".to_owned(),
                    ));
                }

                let commitment = super::hash::hash_preimage(preimage_hex)?;

                if commitment.as_hex() != expected_hash {
                    return Err(CoreError::Adapter(
                        "Lightning preimage does not hash to the expected commitment".to_owned(),
                    ));
                }
            }
            Self::Chain {
                outpoint,
                amount,
                hash_commitment,
                confirmations,
                script_verified,
                ..
            } => {
                if amount != expected_amount
                    || hash_commitment != expected_hash
                    || outpoint.is_empty()
                    || !script_verified
                    || *confirmations == 0
                {
                    return Err(CoreError::Adapter(
                        "chain evidence does not bind amount, hash, and confirmation".to_owned(),
                    ));
                }
            }
        }

        Ok(())
    }

    pub fn public(&self) -> PublicPaymentEvidence {
        match self {
            Self::Lightning {
                payment_hash,
                amount_msat,
                observed_state,
                ..
            } => PublicPaymentEvidence::Lightning {
                payment_hash: payment_hash.clone(),
                amount_msat: amount_msat.clone(),
                observed_state: observed_state.clone(),
                preimage_verified: true,
            },
            Self::Chain {
                txid,
                outpoint,
                amount,
                asset_hash,
                hash_commitment,
                confirmations,
                script_verified,
            } => PublicPaymentEvidence::Chain {
                txid: txid.clone(),
                outpoint: outpoint.clone(),
                amount: amount.clone(),
                asset_hash: asset_hash.clone(),
                hash_commitment: hash_commitment.clone(),
                confirmations: *confirmations,
                script_verified: *script_verified,
            },
        }
    }
}
