//! Explicit-asset Elements regtest HTLC construction and signing.

use std::str::FromStr;

use bitcoin::ScriptBuf;
use bitcoin::secp256k1::{Secp256k1, SecretKey, ecdsa::Signature};
use elements::confidential::{Asset, Nonce, Value};
use elements::encode::{deserialize, serialize_hex};
use elements::sighash::SighashCache;
use elements::{
    AssetIssuance, EcdsaSighashType, LockTime, OutPoint, Script, Sequence, Transaction, TxIn,
    TxInWitness, TxOut, TxOutWitness, Txid,
};
use sha2::{Digest, Sha256};

use super::contract::validate_htlc_contract;
use super::htlc_script::build_htlc_witness_script;
use super::types::{
    AssetId, Chain, ChainError, HtlcContract, LiquidUtxo, SignedTransaction, SpendKind,
};

/// Arguments for constructing an explicit-asset HTLC funding transaction.
#[derive(Clone, Debug)]
pub struct LiquidFundingArgs {
    /// Explicit payment-asset wallet input.
    pub payment_input: LiquidUtxo,
    /// Separate explicit LBTC fee input.
    pub fee_input: LiquidUtxo,
    /// HTLC contract receiving the payment asset.
    pub contract: HtlcContract,
    /// Amount sent to the HTLC output.
    pub htlc_amount_sats: u64,
    /// Payment-asset change script.
    pub payment_change_script: ScriptBuf,
    /// Payment-asset change amount.
    pub payment_change_amount_sats: u64,
    /// Fee-asset change script.
    pub fee_change_script: ScriptBuf,
    /// Fee-asset change amount.
    pub fee_change_amount_sats: u64,
    /// Explicit LBTC miner fee.
    pub fee_sats: u64,
    /// Explicit fee asset, normally LBTC.
    pub fee_asset_id: AssetId,
}

/// Arguments for constructing an explicit-asset HTLC claim or refund.
#[derive(Clone, Debug)]
pub struct LiquidSpendArgs {
    /// HTLC contract being spent.
    pub contract: HtlcContract,
    /// Funding transaction identifier.
    pub funding_txid: Txid,
    /// Funding output index.
    pub funding_vout: u32,
    /// Exact funding output amount.
    pub funding_amount_sats: u64,
    /// Separate explicit LBTC fee input.
    pub fee_input: LiquidUtxo,
    /// Destination script for the payment asset.
    pub destination_script: ScriptBuf,
    /// Fee-asset change script.
    pub fee_change_script: ScriptBuf,
    /// Fee-asset change amount.
    pub fee_change_amount_sats: u64,
    /// Explicit LBTC miner fee.
    pub fee_sats: u64,
    /// Explicit fee asset, normally LBTC.
    pub fee_asset_id: AssetId,
    /// Claim or refund branch.
    pub kind: SpendKind,
}

/// Constructs an unsigned Liquid HTLC funding transaction.
pub fn build_liquid_funding_transaction(
    args: &LiquidFundingArgs,
) -> Result<Transaction, ChainError> {
    validate_funding_args(args)?;
    let payment_asset = explicit_asset(args.contract.asset_id)?;
    let fee_asset = explicit_asset(args.fee_asset_id)?;
    let output_script =
        parse_elements_script(&args.contract.output_script_hex, "HTLC output script")?;
    let payment_outpoint = parse_outpoint(&args.payment_input)?;
    let fee_outpoint = parse_outpoint(&args.fee_input)?;

    Ok(Transaction {
        version: 2,
        lock_time: LockTime::from_consensus(0),
        input: vec![
            new_input(payment_outpoint, 0xffff_fffd),
            new_input(fee_outpoint, 0xffff_fffd),
        ],
        output: funding_outputs(args, payment_asset, fee_asset, output_script),
    })
}

/// Signs both ordinary explicit-input funding witnesses in Rust.
pub fn sign_liquid_funding_transaction(
    mut transaction: Transaction,
    payment_input: &LiquidUtxo,
    fee_input: &LiquidUtxo,
    payment_private_key: &SecretKey,
    fee_private_key: &SecretKey,
) -> Result<SignedTransaction, ChainError> {
    if transaction.input.len() != 2 {
        return Err(ChainError::Validation(
            "Liquid funding transaction must contain payment and fee inputs".to_owned(),
        ));
    }

    if transaction.input[0].previous_output != parse_outpoint(payment_input)?
        || transaction.input[1].previous_output != parse_outpoint(fee_input)?
    {
        return Err(ChainError::Validation(
            "Liquid funding inputs do not match the selected UTXOs".to_owned(),
        ));
    }

    sign_p2wpkh_input(&mut transaction, 0, payment_input, payment_private_key)?;
    sign_p2wpkh_input(&mut transaction, 1, fee_input, fee_private_key)?;

    signed_transaction(transaction)
}

/// Constructs an unsigned Liquid HTLC claim or absolute-height refund.
pub fn build_liquid_htlc_spend(args: &LiquidSpendArgs) -> Result<Transaction, ChainError> {
    validate_spend_args(args)?;
    let payment_asset = explicit_asset(args.contract.asset_id)?;
    let fee_asset = explicit_asset(args.fee_asset_id)?;
    let funding_outpoint = OutPoint::new(args.funding_txid, args.funding_vout);
    let fee_outpoint = parse_outpoint(&args.fee_input)?;
    let destination_script = elements_script(&args.destination_script);

    Ok(Transaction {
        version: 2,
        lock_time: spend_lock_time(args.kind, args.contract.refund_lock_height)?,
        input: vec![
            new_input(funding_outpoint, spend_sequence(args.kind)),
            new_input(fee_outpoint, 0xffff_fffd),
        ],
        output: spend_outputs(args, payment_asset, fee_asset, destination_script),
    })
}

/// Constructs the claim branch transaction and requires the claim kind.
pub fn claim_liquid_htlc(args: &LiquidSpendArgs) -> Result<Transaction, ChainError> {
    if args.kind != SpendKind::Claim {
        return Err(ChainError::InvalidInput(
            "claim_liquid_htlc requires the claim branch".to_owned(),
        ));
    }

    build_liquid_htlc_spend(args)
}

/// Constructs the refund branch transaction and requires the refund kind.
pub fn refund_liquid_htlc(args: &LiquidSpendArgs) -> Result<Transaction, ChainError> {
    if args.kind != SpendKind::Refund {
        return Err(ChainError::InvalidInput(
            "refund_liquid_htlc requires the refund branch".to_owned(),
        ));
    }

    build_liquid_htlc_spend(args)
}

/// Signs the HTLC branch and the separate LBTC fee input in Rust.
pub fn sign_liquid_htlc_spend(
    mut transaction: Transaction,
    contract: &HtlcContract,
    fee_input: &LiquidUtxo,
    htlc_private_key: &SecretKey,
    fee_private_key: &SecretKey,
    funding_amount_sats: u64,
    preimage: Option<[u8; 32]>,
) -> Result<SignedTransaction, ChainError> {
    validate_liquid_contract(contract)?;
    if transaction.input.len() != 2 {
        return Err(ChainError::Validation(
            "Liquid HTLC spend must contain HTLC and fee inputs".to_owned(),
        ));
    }

    if transaction.input[1].previous_output != parse_outpoint(fee_input)? {
        return Err(ChainError::Validation(
            "Liquid fee input does not match the selected UTXO".to_owned(),
        ));
    }

    let witness_script = expected_witness_script(contract)?;
    let expected_key = branch_key(contract, preimage)?;
    let secp = Secp256k1::new();
    let htlc_public_key = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, htlc_private_key);

    if htlc_public_key != expected_key {
        return Err(ChainError::Signature(
            "HTLC signing key does not match the selected Liquid branch".to_owned(),
        ));
    }

    let sighash = liquid_sighash(&mut transaction, 0, &witness_script, funding_amount_sats);
    let signature = sign_sighash(sighash, htlc_private_key)?;
    transaction.input[0].witness.script_witness =
        branch_witness(&witness_script, &signature, preimage);
    sign_p2wpkh_input(&mut transaction, 1, fee_input, fee_private_key)?;

    signed_transaction(transaction)
}

/// Extracts and verifies a claim preimage from a signed Liquid witness.
pub fn extract_liquid_claim_preimage(
    raw_hex: &str,
    contract: &HtlcContract,
    input_index: usize,
) -> Result<[u8; 32], ChainError> {
    validate_liquid_contract(contract)?;
    let bytes = hex::decode(raw_hex).map_err(|_| {
        ChainError::Serialization("Liquid transaction is not hexadecimal".to_owned())
    })?;
    let transaction: Transaction =
        deserialize(&bytes).map_err(|error| ChainError::Serialization(error.to_string()))?;
    let witness = transaction
        .input
        .get(input_index)
        .ok_or_else(|| ChainError::Validation("claim input index is out of range".to_owned()))?
        .witness
        .script_witness
        .to_vec();

    if witness.len() != 4
        || witness[2] != [1]
        || witness[3] != expected_witness_script(contract)?.to_bytes()
    {
        return Err(ChainError::Validation(
            "Liquid claim witness does not match the HTLC branch".to_owned(),
        ));
    }

    let preimage: [u8; 32] = witness[1].as_slice().try_into().map_err(|_| {
        ChainError::Validation("Liquid claim witness preimage must contain 32 bytes".to_owned())
    })?;
    validate_preimage(contract, preimage)?;

    Ok(preimage)
}

fn validate_funding_args(args: &LiquidFundingArgs) -> Result<(), ChainError> {
    validate_liquid_contract(&args.contract)?;
    if args.fee_sats == 0 {
        return Err(ChainError::InvalidInput(
            "Liquid funding requires a positive explicit fee".to_owned(),
        ));
    }

    validate_asset_match(
        args.payment_input.asset_id,
        args.contract.asset_id,
        "payment input",
    )?;
    validate_fee_asset(args.fee_asset_id, args.fee_input.asset_id)?;
    if same_outpoint(&args.payment_input, &args.fee_input) {
        return Err(ChainError::InvalidInput(
            "Liquid payment and fee inputs must be distinct outpoints".to_owned(),
        ));
    }

    let payment_total = args
        .htlc_amount_sats
        .checked_add(args.payment_change_amount_sats)
        .ok_or_else(|| ChainError::InvalidInput("Liquid payment amount overflow".to_owned()))?;
    let fee_total = args
        .fee_change_amount_sats
        .checked_add(args.fee_sats)
        .ok_or_else(|| ChainError::InvalidInput("Liquid fee amount overflow".to_owned()))?;

    if args.payment_input.amount_sats != payment_total || args.fee_input.amount_sats != fee_total {
        return Err(ChainError::InvalidInput(
            "Liquid input amounts must equal their explicit outputs".to_owned(),
        ));
    }

    Ok(())
}

fn validate_spend_args(args: &LiquidSpendArgs) -> Result<(), ChainError> {
    validate_liquid_contract(&args.contract)?;
    if args.fee_sats == 0 || args.funding_amount_sats == 0 {
        return Err(ChainError::InvalidInput(
            "Liquid spend requires positive funding and fee amounts".to_owned(),
        ));
    }

    validate_fee_asset(args.fee_asset_id, args.fee_input.asset_id)?;
    let fee_total = args
        .fee_change_amount_sats
        .checked_add(args.fee_sats)
        .ok_or_else(|| ChainError::InvalidInput("Liquid fee amount overflow".to_owned()))?;

    if args.fee_input.amount_sats != fee_total {
        return Err(ChainError::InvalidInput(
            "Liquid fee input must equal fee change plus fee output".to_owned(),
        ));
    }

    if args.kind == SpendKind::Refund && args.contract.refund_lock_height == 0 {
        return Err(ChainError::InvalidInput(
            "Liquid refund lock height must be positive".to_owned(),
        ));
    }

    Ok(())
}

fn validate_liquid_contract(contract: &HtlcContract) -> Result<(), ChainError> {
    validate_htlc_contract(contract)?;
    if contract.chain != Chain::LiquidRegtest || contract.asset_id == AssetId::Bitcoin {
        return Err(ChainError::WrongNetwork(
            "Liquid adapter requires liquid-regtest and an explicit asset".to_owned(),
        ));
    }

    Ok(())
}

fn validate_asset_match(actual: AssetId, expected: AssetId, label: &str) -> Result<(), ChainError> {
    if actual != expected {
        return Err(ChainError::Validation(format!(
            "{label} asset does not match the HTLC asset"
        )));
    }

    Ok(())
}

fn validate_fee_asset(expected: AssetId, actual: AssetId) -> Result<(), ChainError> {
    if expected == AssetId::Bitcoin || actual != expected {
        return Err(ChainError::InvalidInput(
            "Liquid fee asset must be the matching explicit asset id".to_owned(),
        ));
    }

    Ok(())
}

fn explicit_asset(asset_id: AssetId) -> Result<elements::AssetId, ChainError> {
    let text = asset_id.explicit_bytes().map(hex::encode)?;

    elements::AssetId::from_str(&text).map_err(|_| {
        ChainError::InvalidInput("asset id is not a valid Elements asset hash".to_owned())
    })
}

fn parse_outpoint(input: &LiquidUtxo) -> Result<OutPoint, ChainError> {
    let txid = Txid::from_str(&input.txid)
        .map_err(|_| ChainError::InvalidInput("Liquid funding txid is malformed".to_owned()))?;

    Ok(OutPoint::new(txid, input.vout))
}

fn same_outpoint(left: &LiquidUtxo, right: &LiquidUtxo) -> bool {
    left.txid == right.txid && left.vout == right.vout
}

fn new_input(outpoint: OutPoint, sequence: u32) -> TxIn {
    TxIn {
        previous_output: outpoint,
        is_pegin: false,
        script_sig: Script::new(),
        sequence: Sequence::from_consensus(sequence),
        asset_issuance: AssetIssuance::default(),
        witness: TxInWitness::default(),
    }
}

fn funding_outputs(
    args: &LiquidFundingArgs,
    payment_asset: elements::AssetId,
    fee_asset: elements::AssetId,
    output_script: Script,
) -> Vec<TxOut> {
    let mut outputs = vec![payment_output(
        payment_asset,
        args.htlc_amount_sats,
        output_script,
    )];

    append_output(
        &mut outputs,
        payment_asset,
        args.payment_change_amount_sats,
        &args.payment_change_script,
    );
    append_output(
        &mut outputs,
        fee_asset,
        args.fee_change_amount_sats,
        &args.fee_change_script,
    );
    outputs.push(TxOut::new_fee(args.fee_sats, fee_asset));

    outputs
}

fn spend_outputs(
    args: &LiquidSpendArgs,
    payment_asset: elements::AssetId,
    fee_asset: elements::AssetId,
    destination_script: Script,
) -> Vec<TxOut> {
    let mut outputs = vec![payment_output(
        payment_asset,
        args.funding_amount_sats,
        destination_script,
    )];

    append_output(
        &mut outputs,
        fee_asset,
        args.fee_change_amount_sats,
        &args.fee_change_script,
    );
    outputs.push(TxOut::new_fee(args.fee_sats, fee_asset));

    outputs
}

fn payment_output(asset: elements::AssetId, amount: u64, script: Script) -> TxOut {
    TxOut {
        asset: Asset::Explicit(asset),
        value: Value::Explicit(amount),
        nonce: Nonce::Null,
        script_pubkey: script,
        witness: TxOutWitness::default(),
    }
}

fn append_output(
    outputs: &mut Vec<TxOut>,
    asset: elements::AssetId,
    amount: u64,
    script: &ScriptBuf,
) {
    if amount > 0 {
        outputs.push(payment_output(asset, amount, elements_script(script)));
    }
}

fn sign_p2wpkh_input(
    transaction: &mut Transaction,
    input_index: usize,
    input: &LiquidUtxo,
    private_key: &SecretKey,
) -> Result<(), ChainError> {
    let secp = Secp256k1::new();
    let public_key = bitcoin::PublicKey::new(bitcoin::secp256k1::PublicKey::from_secret_key(
        &secp,
        private_key,
    ));
    let compressed = bitcoin::key::CompressedPublicKey::try_from(public_key)
        .map_err(|_| ChainError::Signature("fee key must be compressed".to_owned()))?;
    let expected_script = ScriptBuf::new_p2wpkh(&compressed.wpubkey_hash());

    if expected_script != input.script_pubkey {
        return Err(ChainError::Signature(
            "Liquid wallet key does not match the input script".to_owned(),
        ));
    }

    let script_code = expected_script
        .p2wpkh_script_code()
        .ok_or_else(|| ChainError::Signature("wallet input is not P2WPKH".to_owned()))?;
    let sighash = liquid_sighash(
        transaction,
        input_index,
        &elements_script(&script_code),
        input.amount_sats,
    );
    let signature = sign_sighash(sighash, private_key)?;
    transaction.input[input_index].witness.script_witness = elements::Witness::from_slice(&[
        encoded_signature(&signature),
        public_key.inner.serialize().to_vec(),
    ]);

    Ok(())
}

fn liquid_sighash(
    transaction: &mut Transaction,
    input_index: usize,
    script_code: &Script,
    amount_sats: u64,
) -> [u8; 32] {
    let mut cache = SighashCache::new(transaction);
    cache
        .segwitv0_sighash(
            input_index,
            script_code,
            Value::Explicit(amount_sats),
            EcdsaSighashType::All,
        )
        .to_byte_array()
}

fn sign_sighash(digest: [u8; 32], private_key: &SecretKey) -> Result<Signature, ChainError> {
    let key = elements::secp256k1_zkp::SecretKey::from_slice(&private_key.secret_bytes())
        .map_err(|error| ChainError::Signature(error.to_string()))?;
    let message = elements::secp256k1_zkp::Message::from_digest_slice(&digest)
        .map_err(|error| ChainError::Signature(error.to_string()))?;
    let signature = elements::secp256k1_zkp::Secp256k1::new().sign_ecdsa(&message, &key);

    Signature::from_der(&signature.serialize_der())
        .map_err(|error| ChainError::Signature(error.to_string()))
}

fn encoded_signature(signature: &Signature) -> Vec<u8> {
    let mut bytes = signature.serialize_der().to_vec();
    bytes.push(EcdsaSighashType::All as u8);

    bytes
}

fn branch_key(
    contract: &HtlcContract,
    preimage: Option<[u8; 32]>,
) -> Result<bitcoin::secp256k1::PublicKey, ChainError> {
    match preimage {
        Some(preimage) => {
            validate_preimage(contract, preimage)?;
            Ok(contract.claim_pubkey)
        }
        None => Ok(contract.refund_pubkey),
    }
}

fn branch_witness(
    witness_script: &Script,
    signature: &Signature,
    preimage: Option<[u8; 32]>,
) -> elements::Witness {
    match preimage {
        Some(preimage) => elements::Witness::from_slice(&[
            encoded_signature(signature),
            preimage.to_vec(),
            vec![1],
            witness_script.to_bytes(),
        ]),
        None => elements::Witness::from_slice(&[
            encoded_signature(signature),
            Vec::new(),
            witness_script.to_bytes(),
        ]),
    }
}

fn spend_lock_time(kind: SpendKind, refund_lock_height: u64) -> Result<LockTime, ChainError> {
    match kind {
        SpendKind::Claim => Ok(LockTime::from_consensus(0)),
        SpendKind::Refund => u32::try_from(refund_lock_height)
            .map(LockTime::from_consensus)
            .map_err(|_| ChainError::InvalidInput("refund lock height exceeds u32".to_owned())),
    }
}

fn spend_sequence(kind: SpendKind) -> u32 {
    match kind {
        SpendKind::Claim => 0xffff_fffd,
        SpendKind::Refund => 0xffff_fffe,
    }
}

fn expected_witness_script(contract: &HtlcContract) -> Result<Script, ChainError> {
    let witness_script = build_htlc_witness_script(
        contract.hash_lock,
        &contract.claim_pubkey,
        &contract.refund_pubkey,
        contract.refund_lock_height,
    )?;

    Ok(elements_script(&witness_script))
}

fn parse_elements_script(value: &str, label: &str) -> Result<Script, ChainError> {
    elements::Script::from_hex_no_prefix(value)
        .map_err(|_| ChainError::InvalidInput(format!("{label} is not hexadecimal")))
}

fn elements_script(script: &ScriptBuf) -> Script {
    Script::from(script.as_bytes().to_vec())
}

fn validate_preimage(contract: &HtlcContract, preimage: [u8; 32]) -> Result<(), ChainError> {
    if Sha256::digest(preimage).as_slice() != contract.hash_lock {
        return Err(ChainError::Validation(
            "preimage does not match the Liquid HTLC hash commitment".to_owned(),
        ));
    }

    Ok(())
}

fn signed_transaction(transaction: Transaction) -> Result<SignedTransaction, ChainError> {
    if transaction
        .input
        .iter()
        .any(|input| input.witness.is_empty())
    {
        return Err(ChainError::Validation(
            "Liquid transaction contains an unsigned input".to_owned(),
        ));
    }

    Ok(SignedTransaction {
        raw_hex: serialize_hex(&transaction),
        txid: transaction.txid().to_string(),
    })
}
