//! Regtest RPC inspection, guarded broadcast, and confirmed claim observation.

use bitcoin::consensus::encode::deserialize as deserialize_bitcoin;
use serde::Deserialize;
use serde_json::{Value, json};

use super::bitcoin::extract_claim_preimage;
use super::contract::validate_htlc_contract;
use super::liquid::extract_liquid_claim_preimage;
use super::types::{AssetId, Chain, ChainError, ClaimPreimage, FundingEvidence, HtlcContract};
use crate::rpc::{RpcClient, RpcError};

/// Arguments for inspecting a confirmed Bitcoin HTLC UTXO.
pub struct InspectBitcoinHtlcArgs<'a> {
    /// Authenticated Bitcoin Core RPC client.
    pub rpc: &'a RpcClient,
    /// Funding transaction identifier.
    pub txid: &'a str,
    /// Funding output index.
    pub vout: u32,
    /// Deterministic HTLC metadata.
    pub contract: &'a HtlcContract,
    /// Expected native amount in satoshis.
    pub expected_amount_sats: u64,
    /// Minimum funding confirmations.
    pub min_confirmations: u32,
    /// Required blocks before the refund height.
    pub min_headroom_blocks: u64,
}

/// Arguments for inspecting a confirmed explicit Liquid HTLC UTXO.
pub struct InspectLiquidHtlcArgs<'a> {
    /// Authenticated Elements RPC client.
    pub rpc: &'a RpcClient,
    /// Funding transaction identifier.
    pub txid: &'a str,
    /// Funding output index.
    pub vout: u32,
    /// Deterministic HTLC metadata.
    pub contract: &'a HtlcContract,
    /// Expected explicit amount in satoshis.
    pub expected_amount_sats: u64,
    /// Minimum funding confirmations.
    pub min_confirmations: u32,
    /// Required blocks before the refund height.
    pub min_headroom_blocks: u64,
}

/// Arguments for a guarded Bitcoin raw transaction broadcast.
pub struct BroadcastBitcoinArgs<'a> {
    /// Authenticated Bitcoin Core RPC client.
    pub rpc: &'a RpcClient,
    /// Fully signed raw transaction.
    pub raw_hex: &'a str,
}

/// Arguments for a guarded Liquid raw transaction broadcast.
pub struct BroadcastLiquidArgs<'a> {
    /// Authenticated Elements RPC client.
    pub rpc: &'a RpcClient,
    /// Fully signed raw transaction.
    pub raw_hex: &'a str,
}

/// Arguments for observing a confirmed Bitcoin claim.
pub struct ObserveBitcoinClaimArgs<'a> {
    /// Authenticated Bitcoin Core RPC client.
    pub rpc: &'a RpcClient,
    /// Expected funding transaction identifier.
    pub funding_txid: &'a str,
    /// Expected funding output index.
    pub funding_vout: u32,
    /// Claim transaction identifier.
    pub claim_txid: &'a str,
    /// Deterministic HTLC metadata.
    pub contract: &'a HtlcContract,
    /// Minimum claim confirmations.
    pub min_confirmations: u32,
}

/// Arguments for observing a confirmed Liquid claim.
pub struct ObserveLiquidClaimArgs<'a> {
    /// Authenticated Elements RPC client.
    pub rpc: &'a RpcClient,
    /// Expected funding transaction identifier.
    pub funding_txid: &'a str,
    /// Expected funding output index.
    pub funding_vout: u32,
    /// Claim transaction identifier.
    pub claim_txid: &'a str,
    /// Deterministic HTLC metadata.
    pub contract: &'a HtlcContract,
    /// Minimum claim confirmations.
    pub min_confirmations: u32,
}

/// Public result for a guarded broadcast.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastEvidence {
    /// Deterministic transaction identifier.
    pub txid: String,
    /// Whether the node already had the transaction during reconciliation.
    pub already_known: bool,
}

/// Public result for a confirmed claim observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaimObservation {
    /// Claim transaction identifier.
    pub claim_txid: String,
    /// Preimage extracted from the confirmed spender witness.
    pub preimage: ClaimPreimage,
}

/// Inspects a Bitcoin funding output and proves it is an unspent confirmed HTLC.
pub fn inspect_bitcoin_htlc_output(
    args: &InspectBitcoinHtlcArgs<'_>,
) -> Result<FundingEvidence, ChainError> {
    validate_inspection_inputs(args.contract, Chain::BitcoinRegtest, args.min_confirmations)?;
    let blockchain: BlockchainInfo = args.rpc.call("getblockchaininfo", &[])?;
    validate_bitcoin_chain(&blockchain)?;
    validate_headroom(
        blockchain.blocks,
        args.contract.refund_lock_height,
        args.min_headroom_blocks,
    )?;
    let transaction: BitcoinVerboseTransaction = args
        .rpc
        .call("getrawtransaction", &[json!(args.txid), json!(true)])?;
    let output = transaction
        .vout
        .iter()
        .find(|item| item.n == args.vout)
        .ok_or_else(|| {
            ChainError::Validation("Bitcoin transaction lacks the requested output".to_owned())
        })?;
    let current: Option<BitcoinGetTxOut> = args.rpc.call(
        "gettxout",
        &[json!(args.txid), json!(args.vout), json!(true)],
    )?;
    let current = current.ok_or_else(|| {
        ChainError::Validation("Bitcoin HTLC output is missing or already spent".to_owned())
    })?;
    validate_bitcoin_output(output, &current, args.contract, args.expected_amount_sats)?;
    let confirmations = confirmed_count(transaction.confirmations)?;

    if confirmations < args.min_confirmations || current.confirmations < args.min_confirmations {
        return Err(ChainError::Validation(
            "Bitcoin HTLC output is not sufficiently confirmed".to_owned(),
        ));
    }

    Ok(FundingEvidence {
        txid: args.txid.to_owned(),
        vout: args.vout,
        amount_sats: args.expected_amount_sats,
        asset_id: AssetId::Bitcoin,
        confirmations,
    })
}

/// Inspects an explicit Liquid funding output and proves its asset and amount.
pub fn inspect_liquid_htlc_output(
    args: &InspectLiquidHtlcArgs<'_>,
) -> Result<FundingEvidence, ChainError> {
    validate_inspection_inputs(args.contract, Chain::LiquidRegtest, args.min_confirmations)?;
    let blockchain: BlockchainInfo = args.rpc.call("getblockchaininfo", &[])?;
    validate_liquid_chain(&blockchain)?;
    validate_headroom(
        blockchain.blocks,
        args.contract.refund_lock_height,
        args.min_headroom_blocks,
    )?;
    let transaction: LiquidVerboseTransaction = args
        .rpc
        .call("getrawtransaction", &[json!(args.txid), json!(true)])?;
    let output = transaction
        .vout
        .iter()
        .find(|item| item.n == args.vout)
        .ok_or_else(|| {
            ChainError::Validation("Elements transaction lacks the requested output".to_owned())
        })?;
    let current: Option<LiquidGetTxOut> = args.rpc.call(
        "gettxout",
        &[json!(args.txid), json!(args.vout), json!(true)],
    )?;
    let current = current.ok_or_else(|| {
        ChainError::Validation("Elements HTLC output is missing or already spent".to_owned())
    })?;
    validate_liquid_output(output, &current, args.contract, args.expected_amount_sats)?;
    let confirmations = confirmed_count(transaction.confirmations)?;

    if confirmations < args.min_confirmations || current.confirmations < args.min_confirmations {
        return Err(ChainError::Validation(
            "Elements HTLC output is not sufficiently confirmed".to_owned(),
        ));
    }

    Ok(FundingEvidence {
        txid: args.txid.to_owned(),
        vout: args.vout,
        amount_sats: args.expected_amount_sats,
        asset_id: args.contract.asset_id,
        confirmations,
    })
}

/// Broadcasts a Bitcoin transaction only after proving the endpoint is regtest.
pub fn broadcast_bitcoin_transaction(
    args: &BroadcastBitcoinArgs<'_>,
) -> Result<BroadcastEvidence, ChainError> {
    let blockchain: BlockchainInfo = args.rpc.call("getblockchaininfo", &[])?;
    validate_bitcoin_chain(&blockchain)?;
    let raw = decode_hex(args.raw_hex, "Bitcoin transaction")?;
    let transaction: bitcoin::Transaction =
        deserialize_bitcoin(&raw).map_err(|error| ChainError::Serialization(error.to_string()))?;
    let expected_txid = transaction.compute_txid().to_string();
    broadcast_and_reconcile(args.rpc, &raw, &expected_txid)
}

/// Broadcasts an Elements transaction only after proving the endpoint is Liquid regtest.
pub fn broadcast_liquid_transaction(
    args: &BroadcastLiquidArgs<'_>,
) -> Result<BroadcastEvidence, ChainError> {
    let blockchain: BlockchainInfo = args.rpc.call("getblockchaininfo", &[])?;
    validate_liquid_chain(&blockchain)?;
    let raw = decode_hex(args.raw_hex, "Liquid transaction")?;
    let transaction: elements::Transaction = elements::encode::deserialize(&raw)
        .map_err(|error| ChainError::Serialization(error.to_string()))?;
    let expected_txid = transaction.txid().to_string();
    broadcast_and_reconcile(args.rpc, &raw, &expected_txid)
}

/// Observes a confirmed Bitcoin claim and extracts the witness preimage only after outpoint binding.
pub fn observe_bitcoin_claim(
    args: &ObserveBitcoinClaimArgs<'_>,
) -> Result<ClaimObservation, ChainError> {
    validate_inspection_inputs(args.contract, Chain::BitcoinRegtest, args.min_confirmations)?;
    let blockchain: BlockchainInfo = args.rpc.call("getblockchaininfo", &[])?;
    validate_bitcoin_chain(&blockchain)?;
    let claim: BitcoinVerboseTransaction = args
        .rpc
        .call("getrawtransaction", &[json!(args.claim_txid), json!(true)])?;
    let confirmations = confirmed_count(claim.confirmations)?;

    if confirmations < args.min_confirmations {
        return Err(ChainError::Validation(
            "Bitcoin claim transaction is not sufficiently confirmed".to_owned(),
        ));
    }

    let input_index = find_funding_input(&claim.vin, args.funding_txid, args.funding_vout)?;
    let raw_hex = claim.hex.ok_or_else(|| {
        ChainError::Validation("Bitcoin claim transaction lacks raw hex".to_owned())
    })?;
    let preimage = extract_claim_preimage(&raw_hex, args.contract, input_index)?;

    Ok(ClaimObservation {
        claim_txid: args.claim_txid.to_owned(),
        preimage: ClaimPreimage {
            preimage,
            confirmations,
        },
    })
}

/// Observes a confirmed Liquid claim and extracts the witness preimage only after outpoint binding.
pub fn observe_liquid_claim(
    args: &ObserveLiquidClaimArgs<'_>,
) -> Result<ClaimObservation, ChainError> {
    validate_inspection_inputs(args.contract, Chain::LiquidRegtest, args.min_confirmations)?;
    let blockchain: BlockchainInfo = args.rpc.call("getblockchaininfo", &[])?;
    validate_liquid_chain(&blockchain)?;
    let claim: LiquidVerboseTransaction = args
        .rpc
        .call("getrawtransaction", &[json!(args.claim_txid), json!(true)])?;
    let confirmations = confirmed_count(claim.confirmations)?;

    if confirmations < args.min_confirmations {
        return Err(ChainError::Validation(
            "Liquid claim transaction is not sufficiently confirmed".to_owned(),
        ));
    }

    let input_index = find_funding_input(&claim.vin, args.funding_txid, args.funding_vout)?;
    let raw_hex = claim.hex.ok_or_else(|| {
        ChainError::Validation("Liquid claim transaction lacks raw hex".to_owned())
    })?;
    let preimage = extract_liquid_claim_preimage(&raw_hex, args.contract, input_index)?;

    Ok(ClaimObservation {
        claim_txid: args.claim_txid.to_owned(),
        preimage: ClaimPreimage {
            preimage,
            confirmations,
        },
    })
}

/// Parses a Core or Elements coin amount without floating-point conversion.
pub fn parse_rpc_coin_amount(value: &Value) -> Result<u64, ChainError> {
    parse_coin_amount(value)
}

fn validate_inspection_inputs(
    contract: &HtlcContract,
    expected_chain: Chain,
    min_confirmations: u32,
) -> Result<(), ChainError> {
    validate_htlc_contract(contract)?;

    if contract.chain != expected_chain {
        return Err(ChainError::WrongNetwork(
            "HTLC contract chain does not match the inspection adapter".to_owned(),
        ));
    }

    if min_confirmations == 0 {
        return Err(ChainError::InvalidInput(
            "minimum confirmations must be positive".to_owned(),
        ));
    }

    Ok(())
}

fn validate_headroom(
    blocks: u64,
    refund_lock_height: u64,
    min_headroom_blocks: u64,
) -> Result<(), ChainError> {
    let required = blocks
        .checked_add(min_headroom_blocks)
        .ok_or_else(|| ChainError::Validation("chain height overflow".to_owned()))?;

    if required >= refund_lock_height {
        return Err(ChainError::RefundTooEarly {
            current_height: blocks,
            lock_height: refund_lock_height,
        });
    }

    Ok(())
}

fn validate_bitcoin_chain(info: &BlockchainInfo) -> Result<(), ChainError> {
    if info.chain != "regtest" {
        return Err(ChainError::WrongNetwork(format!(
            "Bitcoin RPC is on {}, expected regtest",
            info.chain
        )));
    }

    Ok(())
}

fn validate_liquid_chain(info: &BlockchainInfo) -> Result<(), ChainError> {
    if info.chain != "elementsregtest" && info.chain != "liquidregtest" {
        return Err(ChainError::WrongNetwork(format!(
            "Elements RPC is on {}, expected Elements regtest",
            info.chain
        )));
    }

    Ok(())
}

fn validate_bitcoin_output(
    output: &BitcoinVerboseOutput,
    current: &BitcoinGetTxOut,
    contract: &HtlcContract,
    expected_amount_sats: u64,
) -> Result<(), ChainError> {
    if output.script_pub_key.hex != contract.output_script_hex
        || current.script_pub_key.hex != contract.output_script_hex
    {
        return Err(ChainError::Validation(
            "Bitcoin HTLC output script does not match the contract".to_owned(),
        ));
    }

    if parse_coin_amount(&output.value)? != expected_amount_sats
        || parse_coin_amount(&current.value)? != expected_amount_sats
    {
        return Err(ChainError::Validation(
            "Bitcoin HTLC output amount does not match the expected amount".to_owned(),
        ));
    }

    Ok(())
}

fn validate_liquid_output(
    output: &LiquidVerboseOutput,
    current: &LiquidGetTxOut,
    contract: &HtlcContract,
    expected_amount_sats: u64,
) -> Result<(), ChainError> {
    let expected_asset = contract.asset_id.to_hex();

    if output.script_pub_key.hex != contract.output_script_hex
        || current.script_pub_key.hex != contract.output_script_hex
        || output.asset != expected_asset
        || current.asset != expected_asset
    {
        return Err(ChainError::Validation(
            "Elements HTLC output script or asset does not match the contract".to_owned(),
        ));
    }

    if parse_coin_amount(&output.value)? != expected_amount_sats
        || parse_coin_amount(&current.value)? != expected_amount_sats
    {
        return Err(ChainError::Validation(
            "Elements HTLC output amount does not match the expected amount".to_owned(),
        ));
    }

    Ok(())
}

fn confirmed_count(value: Option<i64>) -> Result<u32, ChainError> {
    let confirmations = value.unwrap_or(0);

    if confirmations < 0 {
        return Err(ChainError::Validation(
            "transaction is conflicted or unconfirmed".to_owned(),
        ));
    }

    u32::try_from(confirmations).map_err(|_| {
        ChainError::Validation("confirmation count exceeds supported range".to_owned())
    })
}

fn parse_coin_amount(value: &Value) -> Result<u64, ChainError> {
    let text = match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => {
            return Err(ChainError::Validation(
                "RPC amount must be a decimal string or JSON number".to_owned(),
            ));
        }
    };
    parse_decimal_sats(&text)
}

fn parse_decimal_sats(value: &str) -> Result<u64, ChainError> {
    if value.is_empty() || value.starts_with('-') || value.contains('e') || value.contains('E') {
        return Err(ChainError::Validation(
            "RPC amount must be a non-negative fixed decimal".to_owned(),
        ));
    }

    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fractional = parts.next().unwrap_or_default();

    if parts.next().is_some()
        || whole.is_empty()
        || !whole.chars().all(|character| character.is_ascii_digit())
        || !fractional
            .chars()
            .all(|character| character.is_ascii_digit())
        || fractional.len() > 8
    {
        return Err(ChainError::Validation(
            "RPC amount must contain at most 8 decimal places".to_owned(),
        ));
    }

    let whole_sats = whole
        .parse::<u64>()
        .map_err(|_| ChainError::Validation("RPC amount exceeds u64".to_owned()))?
        .checked_mul(100_000_000)
        .ok_or_else(|| ChainError::Validation("RPC amount exceeds u64".to_owned()))?;
    let fractional_sats = format!("{fractional:0<8}")
        .parse::<u64>()
        .map_err(|_| ChainError::Validation("RPC amount fractional part is invalid".to_owned()))?;

    whole_sats
        .checked_add(fractional_sats)
        .ok_or_else(|| ChainError::Validation("RPC amount exceeds u64".to_owned()))
}

fn decode_hex(value: &str, label: &str) -> Result<Vec<u8>, ChainError> {
    if value.is_empty() || !value.len().is_multiple_of(2) {
        return Err(ChainError::Serialization(format!(
            "{label} is not even-length hexadecimal"
        )));
    }

    hex::decode(value).map_err(|_| ChainError::Serialization(format!("{label} is not hexadecimal")))
}

fn broadcast_and_reconcile(
    rpc: &RpcClient,
    raw: &[u8],
    expected_txid: &str,
) -> Result<BroadcastEvidence, ChainError> {
    let raw_hex = hex::encode(raw);
    match rpc.call::<String>("sendrawtransaction", &[json!(raw_hex)]) {
        Ok(returned_txid) if returned_txid == expected_txid => Ok(BroadcastEvidence {
            txid: expected_txid.to_owned(),
            already_known: false,
        }),
        Ok(_) => Err(ChainError::Validation(
            "RPC returned a transaction id different from local serialization".to_owned(),
        )),
        Err(error) => reconcile_known_transaction(rpc, expected_txid, error),
    }
}

fn reconcile_known_transaction(
    rpc: &RpcClient,
    expected_txid: &str,
    broadcast_error: RpcError,
) -> Result<BroadcastEvidence, ChainError> {
    let known: Result<GenericTransaction, RpcError> =
        rpc.call("getrawtransaction", &[json!(expected_txid), json!(true)]);

    match known {
        Ok(transaction) if transaction.txid == expected_txid => Ok(BroadcastEvidence {
            txid: expected_txid.to_owned(),
            already_known: true,
        }),
        _ => Err(ChainError::Validation(format!(
            "transaction broadcast failed: {}",
            sanitize_rpc_error(&broadcast_error)
        ))),
    }
}

fn sanitize_rpc_error(error: &RpcError) -> String {
    let message = error.to_string();
    let mut sanitized = String::with_capacity(message.len());
    let bytes = message.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if index + 64 <= bytes.len()
            && bytes[index..index + 64]
                .iter()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            sanitized.push_str("[txid]");
            index += 64;
            continue;
        }

        sanitized.push(bytes[index] as char);
        index += 1;
    }

    sanitized.chars().take(180).collect()
}

fn find_funding_input(
    inputs: &[GenericInput],
    funding_txid: &str,
    funding_vout: u32,
) -> Result<usize, ChainError> {
    inputs
        .iter()
        .position(|input| {
            input.txid.as_deref() == Some(funding_txid) && input.vout == Some(funding_vout)
        })
        .ok_or_else(|| {
            ChainError::Validation("claim does not spend the expected HTLC outpoint".to_owned())
        })
}

#[derive(Clone, Debug, Deserialize)]
struct BlockchainInfo {
    chain: String,
    blocks: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct BitcoinGetTxOut {
    confirmations: u32,
    value: Value,
    #[serde(rename = "scriptPubKey")]
    script_pub_key: RpcScript,
}

#[derive(Clone, Debug, Deserialize)]
struct BitcoinVerboseOutput {
    n: u32,
    value: Value,
    #[serde(rename = "scriptPubKey")]
    script_pub_key: RpcScript,
}

#[derive(Clone, Debug, Deserialize)]
struct BitcoinVerboseTransaction {
    confirmations: Option<i64>,
    hex: Option<String>,
    vin: Vec<GenericInput>,
    vout: Vec<BitcoinVerboseOutput>,
}

#[derive(Clone, Debug, Deserialize)]
struct LiquidGetTxOut {
    confirmations: u32,
    value: Value,
    asset: String,
    #[serde(rename = "scriptPubKey")]
    script_pub_key: RpcScript,
}

#[derive(Clone, Debug, Deserialize)]
struct LiquidVerboseOutput {
    n: u32,
    value: Value,
    asset: String,
    #[serde(rename = "scriptPubKey")]
    script_pub_key: RpcScript,
}

#[derive(Clone, Debug, Deserialize)]
struct LiquidVerboseTransaction {
    confirmations: Option<i64>,
    hex: Option<String>,
    vin: Vec<GenericInput>,
    vout: Vec<LiquidVerboseOutput>,
}

#[derive(Clone, Debug, Deserialize)]
struct GenericTransaction {
    txid: String,
}

#[derive(Clone, Debug, Deserialize)]
struct GenericInput {
    txid: Option<String>,
    vout: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
struct RpcScript {
    hex: String,
}
