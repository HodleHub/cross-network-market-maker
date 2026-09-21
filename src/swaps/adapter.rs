//! Prepared transaction boundaries used by the durable swap runner.

use std::fmt::{Debug, Formatter};

use bitcoin::secp256k1::PublicKey;

use crate::chain::{AssetId, FundingEvidence, HtlcContract, SpendKind};

use super::error::SwapError;
use super::outbox::{FundingOutbox, OutboxKind};

/// A transaction prepared locally and safe to persist before broadcast.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedTransaction {
    /// Immutable signed transaction record.
    pub outbox: FundingOutbox,
    /// Funding output index for a funding transaction.
    pub funding_vout: Option<u32>,
    /// Asset transferred by the HTLC output.
    pub asset_id: AssetId,
    /// Amount transferred by the HTLC output.
    pub amount_sats: u64,
}

/// A funding request bound to a previously persisted recovery contract.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrepareFundingRequest {
    /// Recovery contract receiving the funds.
    pub contract: HtlcContract,
    /// Exact HTLC amount.
    pub amount_sats: u64,
    /// Explicit fee amount.
    pub fee_sats: u64,
    /// Explicit fee asset.
    pub fee_asset_id: AssetId,
}

/// Private keys needed to sign one HTLC spend.
#[derive(Clone, Eq, PartialEq)]
pub struct SpendKeyMaterial {
    /// Claim or refund branch private key.
    pub branch_private_key: [u8; 32],
    /// Client destination public key.
    pub destination_public_key: PublicKey,
    /// Claim preimage, required only on the claim branch.
    pub preimage: Option<[u8; 32]>,
}

impl Debug for SpendKeyMaterial {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SpendKeyMaterial")
            .field("branch_private_key", &"<redacted>")
            .field("destination_public_key", &self.destination_public_key)
            .field("preimage", &self.preimage.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A claim or refund request against a confirmed HTLC funding output.
#[derive(Clone, Eq, PartialEq)]
pub struct PrepareSpendRequest {
    /// HTLC contract being spent.
    pub contract: HtlcContract,
    /// Confirmed or mempool funding evidence.
    pub funding: FundingEvidence,
    /// Explicit fee amount.
    pub fee_sats: u64,
    /// Explicit fee asset.
    pub fee_asset_id: AssetId,
    /// Selected HTLC branch.
    pub kind: SpendKind,
    /// Branch and destination keys.
    pub keys: SpendKeyMaterial,
}

/// A transaction that was accepted by the node or was already known there.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastEvidence {
    /// Deterministic transaction identifier.
    pub txid: String,
    /// Confirmations observed immediately after submission.
    pub confirmations: u32,
}

/// Confirmed evidence that an HTLC claim revealed the expected preimage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimEvidence {
    /// Claim transaction identifier.
    pub txid: String,
    /// Funding transaction identifier.
    pub funding_txid: String,
    /// Funding output index.
    pub funding_vout: u32,
    /// Confirmations on the claim transaction.
    pub confirmations: u32,
    /// The witness matched the contract hash commitment.
    pub preimage_verified: bool,
}

/// Confirmed evidence that an HTLC refund was accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefundEvidence {
    /// Refund transaction identifier.
    pub txid: String,
    /// Funding transaction identifier.
    pub funding_txid: String,
    /// Funding output index.
    pub funding_vout: u32,
    /// Confirmations on the refund transaction.
    pub confirmations: u32,
}

/// Chain boundary required by the generic atomic swap runner.
pub trait SwapChainAdapter {
    /// Return the current height of the selected regtest chain.
    fn current_height(&self) -> Result<u64, SwapError>;

    /// Build and sign funding without broadcasting it.
    fn prepare_funding(
        &self,
        request: PrepareFundingRequest,
    ) -> Result<PreparedTransaction, SwapError>;

    /// Build and sign a claim or refund without broadcasting it.
    fn prepare_spend(&self, request: PrepareSpendRequest)
    -> Result<PreparedTransaction, SwapError>;

    /// Broadcast an immutable outbox record, treating an already-known tx as replay.
    fn broadcast(&self, outbox: &FundingOutbox) -> Result<BroadcastEvidence, SwapError>;

    /// Mine or wait for one confirmation when the selected regtest adapter supports it.
    fn confirm_funding(
        &self,
        _contract: &HtlcContract,
        _outbox: &FundingOutbox,
    ) -> Result<(), SwapError> {
        Err(SwapError::Chain(
            "funding confirmation requires an adapter-specific regtest hook".to_owned(),
        ))
    }

    /// Mine or wait for one confirmation of a spend transaction.
    fn confirm_spend(&self, _outbox: &FundingOutbox) -> Result<(), SwapError> {
        Err(SwapError::Chain(
            "spend confirmation requires an adapter-specific regtest hook".to_owned(),
        ))
    }

    /// Observe the funding output and bind it to the contract and amount.
    fn observe_funding(
        &self,
        contract: &HtlcContract,
        txid: &str,
        vout: u32,
        amount_sats: u64,
    ) -> Result<FundingEvidence, SwapError>;

    /// Observe and validate a claim witness.
    fn observe_claim(
        &self,
        contract: &HtlcContract,
        outbox: &FundingOutbox,
        expected_preimage: [u8; 32],
    ) -> Result<ClaimEvidence, SwapError>;

    /// Observe a refund transaction.
    fn observe_refund(&self, outbox: &FundingOutbox) -> Result<RefundEvidence, SwapError>;
}

/// Converts a funding preparation into the durable outbox shape used by replay.
pub fn funding_outbox(prepared: &PreparedTransaction) -> Result<&FundingOutbox, SwapError> {
    if prepared.outbox.kind != OutboxKind::Funding {
        return Err(SwapError::InvalidInput(
            "funding preparation returned a non-funding outbox".to_owned(),
        ));
    }

    Ok(&prepared.outbox)
}
