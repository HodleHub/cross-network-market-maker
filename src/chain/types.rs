//! Shared typed values for the Bitcoin and Elements adapters.

use bitcoin::ScriptBuf;
use bitcoin::secp256k1::PublicKey;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::rpc::RpcError;

/// Supported chain environments in this POC.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Chain {
    /// Bitcoin Core regtest.
    BitcoinRegtest,
    /// Elements Core regtest.
    LiquidRegtest,
}

/// Native Bitcoin or one explicit Elements asset identifier.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub enum AssetId {
    /// Bitcoin's native satoshi asset.
    Bitcoin,
    /// A 32-byte explicit Elements asset identifier.
    Explicit([u8; 32]),
}

impl std::fmt::Debug for AssetId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl AssetId {
    /// Parses the canonical lowercase hexadecimal asset identifier.
    pub fn from_hex(value: &str) -> Result<Self, ChainError> {
        if value == "BTC" {
            return Ok(Self::Bitcoin);
        }

        if value.len() != 64
            || value
                .chars()
                .any(|character| !character.is_ascii_hexdigit())
        {
            return Err(ChainError::InvalidInput(
                "asset id must be BTC or 32-byte hexadecimal text".to_owned(),
            ));
        }

        if value
            .chars()
            .any(|character| character.is_ascii_uppercase())
        {
            return Err(ChainError::InvalidInput(
                "asset id hexadecimal must be lowercase".to_owned(),
            ));
        }

        let bytes = hex::decode(value).map_err(|_| {
            ChainError::InvalidInput("asset id must contain hexadecimal bytes".to_owned())
        })?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
            ChainError::InvalidInput("asset id must contain exactly 32 bytes".to_owned())
        })?;

        Ok(Self::Explicit(bytes))
    }

    /// Returns the canonical public representation.
    pub fn to_hex(self) -> String {
        match self {
            Self::Bitcoin => "BTC".to_owned(),
            Self::Explicit(bytes) => hex::encode(bytes),
        }
    }

    /// Returns an explicit Elements asset hash, rejecting native Bitcoin.
    pub fn explicit_bytes(self) -> Result<[u8; 32], ChainError> {
        match self {
            Self::Bitcoin => Err(ChainError::InvalidInput(
                "native Bitcoin has no explicit Elements asset hash".to_owned(),
            )),
            Self::Explicit(bytes) => Ok(bytes),
        }
    }
}

impl Serialize for AssetId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for AssetId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;

        Self::from_hex(&value).map_err(serde::de::Error::custom)
    }
}

/// Deterministic P2WSH HTLC metadata shared by chain settlement and recovery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HtlcContract {
    /// Chain on which this contract may be used.
    pub chain: Chain,
    /// Native Bitcoin or the exact explicit Liquid asset.
    pub asset_id: AssetId,
    /// SHA-256 commitment of the 32-byte claim preimage.
    pub hash_lock: [u8; 32],
    /// Compressed claim branch key.
    pub claim_pubkey: PublicKey,
    /// Compressed absolute-height refund branch key.
    pub refund_pubkey: PublicKey,
    /// Absolute regtest block height for the refund branch.
    pub refund_lock_height: u64,
    /// Witness script in lowercase hexadecimal form.
    pub witness_script_hex: String,
    /// Native P2WSH output script in lowercase hexadecimal form.
    pub output_script_hex: String,
    /// Regtest address corresponding to the output script.
    pub address: String,
}

/// Confirmed evidence that a funding output exists on the selected chain.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct FundingEvidence {
    /// Funding transaction identifier.
    pub txid: String,
    /// Funding output index.
    pub vout: u32,
    /// Exact asset amount in satoshi-like units.
    pub amount_sats: u64,
    /// Exact explicit asset, or native Bitcoin.
    pub asset_id: AssetId,
    /// Confirmations observed at the node tip.
    pub confirmations: u32,
}

/// Result of a locally signed chain transaction.
#[derive(Clone, Eq, PartialEq)]
pub struct SignedTransaction {
    /// Raw transaction bytes ready for a guarded broadcast.
    pub raw_hex: String,
    /// Deterministic transaction identifier.
    pub txid: String,
}

/// Claim or absolute-height refund branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpendKind {
    /// Reveal the preimage and claim with the claim key.
    Claim,
    /// Use the CLTV branch with the refund key.
    Refund,
}

/// Errors at the chain adapter boundary.
#[derive(Debug, Error)]
pub enum ChainError {
    /// A caller supplied a malformed or inconsistent value.
    #[error("invalid chain input: {0}")]
    InvalidInput(String),
    /// The selected endpoint is not the required local regtest node.
    #[error("wrong chain or network: {0}")]
    WrongNetwork(String),
    /// A contract or outpoint did not match the observed node data.
    #[error("chain validation failed: {0}")]
    Validation(String),
    /// RPC transport or node error.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// Transaction or script serialization failed.
    #[error("transaction serialization failed: {0}")]
    Serialization(String),
    /// Key or signature construction failed.
    #[error("signature construction failed: {0}")]
    Signature(String),
    /// The node rejected a refund before the absolute lock height.
    #[error("refund is not yet valid at chain height {current_height}; requires {lock_height}")]
    RefundTooEarly {
        current_height: u64,
        lock_height: u64,
    },
}

/// A caller-owned Bitcoin UTXO used for ordinary funding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BitcoinUtxo {
    /// Previous transaction identifier.
    pub txid: String,
    /// Previous output index.
    pub vout: u32,
    /// Exact satoshi value.
    pub amount_sats: u64,
    /// Previous output script.
    pub script_pubkey: ScriptBuf,
    /// Confirmations observed at selection time.
    pub confirmations: u32,
}

/// A caller-owned explicit Elements UTXO.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiquidUtxo {
    /// Previous transaction identifier.
    pub txid: String,
    /// Previous output index.
    pub vout: u32,
    /// Exact explicit asset identifier.
    pub asset_id: AssetId,
    /// Exact asset amount.
    pub amount_sats: u64,
    /// Previous output script.
    pub script_pubkey: ScriptBuf,
    /// Confirmations observed at selection time.
    pub confirmations: u32,
}

/// Preimage recovered from a confirmed claim witness.
#[derive(Clone, Eq, PartialEq)]
pub struct ClaimPreimage {
    /// Exact 32-byte preimage.
    pub preimage: [u8; 32],
    /// Confirmations for the observed claim transaction.
    pub confirmations: u32,
}

impl std::fmt::Debug for SignedTransaction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SignedTransaction")
            .field("txid", &self.txid)
            .field("raw_hex", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for ClaimPreimage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClaimPreimage")
            .field("preimage", &"<redacted>")
            .field("confirmations", &self.confirmations)
            .finish()
    }
}
