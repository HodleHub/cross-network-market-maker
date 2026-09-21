//! Deterministic hashlock/CLTV witness-script construction.

use bitcoin::ScriptBuf;
use bitcoin::blockdata::script::Builder;
use bitcoin::opcodes::all::{
    OP_CHECKSIG, OP_CLTV, OP_DROP, OP_ELSE, OP_ENDIF, OP_EQUALVERIFY, OP_IF, OP_SHA256, OP_SIZE,
};
use bitcoin::script::PushBytesBuf;
use bitcoin::secp256k1::PublicKey;

use super::types::ChainError;

/// Builds the native SegWit witness script used by both chain adapters.
pub fn build_htlc_witness_script(
    hash_lock: [u8; 32],
    claim_pubkey: &PublicKey,
    refund_pubkey: &PublicKey,
    refund_lock_height: u64,
) -> Result<ScriptBuf, ChainError> {
    validate_lock_height(refund_lock_height)?;
    let lock_height = i64::try_from(refund_lock_height).map_err(|_| {
        ChainError::InvalidInput(
            "refund lock height exceeds signed script integer range".to_owned(),
        )
    })?;
    let hash_bytes = PushBytesBuf::try_from(hash_lock.to_vec())
        .map_err(|_| ChainError::InvalidInput("hash lock must contain 32 bytes".to_owned()))?;
    let claim_bytes = PushBytesBuf::try_from(claim_pubkey.serialize().to_vec()).map_err(|_| {
        ChainError::InvalidInput("claim key must be a compressed public key".to_owned())
    })?;
    let refund_bytes =
        PushBytesBuf::try_from(refund_pubkey.serialize().to_vec()).map_err(|_| {
            ChainError::InvalidInput("refund key must be a compressed public key".to_owned())
        })?;

    Ok(Builder::new()
        .push_opcode(OP_IF)
        .push_opcode(OP_SIZE)
        .push_int(32)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(OP_SHA256)
        .push_slice(hash_bytes)
        .push_opcode(OP_EQUALVERIFY)
        .push_slice(claim_bytes)
        .push_opcode(OP_CHECKSIG)
        .push_opcode(OP_ELSE)
        .push_int(lock_height)
        .push_opcode(OP_CLTV)
        .push_opcode(OP_DROP)
        .push_slice(refund_bytes)
        .push_opcode(OP_CHECKSIG)
        .push_opcode(OP_ENDIF)
        .into_script())
}

/// Rejects timestamp-mode lock values and nonsensical regtest heights.
pub fn validate_lock_height(refund_lock_height: u64) -> Result<(), ChainError> {
    if !(1..500_000_000).contains(&refund_lock_height) {
        return Err(ChainError::InvalidInput(
            "refund lock height must be a positive block height below 500000000".to_owned(),
        ));
    }

    Ok(())
}
