//! Bitcoin regtest HTLC transaction construction and signing.

use std::str::FromStr;

use bitcoin::absolute::LockTime;
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::hashes::Hash;
use bitcoin::key::CompressedPublicKey;
use bitcoin::secp256k1::{Message, Secp256k1, SecretKey, ecdsa::Signature};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};

use super::contract::validate_htlc_contract;
use super::htlc_script::build_htlc_witness_script;
use super::types::{
    AssetId, BitcoinUtxo, Chain, ChainError, HtlcContract, SignedTransaction, SpendKind,
};

/// Arguments for constructing a Bitcoin HTLC funding transaction.
#[derive(Clone, Debug)]
pub struct BitcoinFundingArgs {
    /// Caller-owned wallet input.
    pub input: BitcoinUtxo,
    /// Contract receiving the HTLC amount.
    pub contract: HtlcContract,
    /// Amount sent to the HTLC output.
    pub htlc_amount_sats: u64,
    /// Caller-owned change output script.
    pub change_script: ScriptBuf,
    /// Change amount.
    pub change_amount_sats: u64,
    /// Explicit miner fee.
    pub fee_sats: u64,
}

/// Arguments for constructing a Bitcoin HTLC claim or refund.
#[derive(Clone, Debug)]
pub struct BitcoinSpendArgs {
    /// HTLC contract being spent.
    pub contract: HtlcContract,
    /// Funding transaction identifier.
    pub funding_txid: Txid,
    /// Funding output index.
    pub funding_vout: u32,
    /// Exact funding output value.
    pub funding_amount_sats: u64,
    /// Destination script for the spend output.
    pub destination_script: ScriptBuf,
    /// Explicit miner fee.
    pub fee_sats: u64,
    /// Claim or refund branch.
    pub kind: SpendKind,
}

/// Constructs an unsigned Bitcoin HTLC funding transaction.
pub fn build_bitcoin_funding_transaction(
    args: &BitcoinFundingArgs,
) -> Result<Transaction, ChainError> {
    validate_funding_amounts(args)?;
    validate_bitcoin_contract(&args.contract)?;
    let htlc_script = parse_script(&args.contract.output_script_hex, "HTLC output script")?;
    let txid = parse_txid(&args.input.txid)?;

    Ok(Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid,
                vout: args.input.vout,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::from_consensus(0xffff_fffd),
            witness: Witness::default(),
        }],
        output: funding_outputs(args, htlc_script),
    })
}

/// Signs the ordinary P2WPKH funding input in Rust and extracts the transaction.
pub fn sign_bitcoin_funding_transaction(
    mut transaction: Transaction,
    input: &BitcoinUtxo,
    private_key: &SecretKey,
) -> Result<SignedTransaction, ChainError> {
    if transaction.input.len() != 1 {
        return Err(ChainError::Validation(
            "Bitcoin funding transaction must contain one wallet input".to_owned(),
        ));
    }

    let expected_outpoint = OutPoint {
        txid: parse_txid(&input.txid)?,
        vout: input.vout,
    };

    if transaction.input[0].previous_output != expected_outpoint {
        return Err(ChainError::Validation(
            "Bitcoin funding input does not match the selected UTXO".to_owned(),
        ));
    }

    let secp = Secp256k1::new();
    let public_key =
        bitcoin::PublicKey::new(secp256k1::PublicKey::from_secret_key(&secp, private_key));
    let compressed = CompressedPublicKey::try_from(public_key)
        .map_err(|_| ChainError::Signature("funding key must be compressed".to_owned()))?;
    let expected_script = ScriptBuf::new_p2wpkh(&compressed.wpubkey_hash());

    if expected_script != input.script_pubkey {
        return Err(ChainError::Signature(
            "funding key does not match the wallet input script".to_owned(),
        ));
    }

    let sighash = {
        let mut cache = SighashCache::new(&mut transaction);
        cache
            .p2wpkh_signature_hash(
                0,
                &input.script_pubkey,
                Amount::from_sat(input.amount_sats),
                EcdsaSighashType::All,
            )
            .map_err(|error| ChainError::Signature(error.to_string()))?
    };
    let signature = sign_sighash(sighash.to_byte_array(), private_key)?;
    transaction.input[0].witness = Witness::from_slice(&[
        encoded_signature(&signature),
        public_key.inner.serialize().to_vec(),
    ]);

    signed_transaction(transaction)
}

/// Constructs an unsigned Bitcoin HTLC claim or absolute-height refund.
pub fn build_bitcoin_htlc_spend(args: &BitcoinSpendArgs) -> Result<Transaction, ChainError> {
    validate_bitcoin_contract(&args.contract)?;
    if args.fee_sats == 0 || args.funding_amount_sats <= args.fee_sats {
        return Err(ChainError::InvalidInput(
            "funding amount must exceed a positive fee".to_owned(),
        ));
    }

    let witness_script = parse_script(&args.contract.witness_script_hex, "HTLC witness script")?;
    let output_script = parse_script(&args.contract.output_script_hex, "HTLC output script")?;
    let transaction = Transaction {
        version: Version::TWO,
        lock_time: spend_lock_time(args)?,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: args.funding_txid,
                vout: args.funding_vout,
            },
            script_sig: ScriptBuf::new(),
            sequence: spend_sequence(args.kind),
            witness: Witness::default(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(args.funding_amount_sats - args.fee_sats),
            script_pubkey: args.destination_script.clone(),
        }],
    };

    if output_script != expected_output_script(&args.contract)?
        || witness_script != expected_witness_script(&args.contract)?
    {
        return Err(ChainError::Validation(
            "contract script metadata does not match its deterministic script".to_owned(),
        ));
    }

    Ok(transaction)
}

/// Signs an HTLC branch witness with the matching caller-owned key.
pub fn sign_bitcoin_htlc_spend(
    mut transaction: Transaction,
    contract: &HtlcContract,
    private_key: &SecretKey,
    funding_amount_sats: u64,
    preimage: Option<[u8; 32]>,
) -> Result<SignedTransaction, ChainError> {
    validate_bitcoin_contract(contract)?;
    let witness_script = expected_witness_script(contract)?;
    let secp = Secp256k1::new();
    let public_key = secp256k1::PublicKey::from_secret_key(&secp, private_key);
    let expected_key = match preimage {
        Some(preimage) => {
            validate_preimage(contract, preimage)?;
            contract.claim_pubkey
        }
        None => contract.refund_pubkey,
    };

    if public_key != expected_key {
        return Err(ChainError::Signature(
            "signing key does not match the selected HTLC branch".to_owned(),
        ));
    }

    let sighash = {
        let mut cache = SighashCache::new(&mut transaction);
        cache
            .p2wsh_signature_hash(
                0,
                witness_script.as_script(),
                Amount::from_sat(funding_amount_sats),
                EcdsaSighashType::All,
            )
            .map_err(|error| ChainError::Signature(error.to_string()))?
    };
    let signature = sign_sighash(sighash.to_byte_array(), private_key)?;
    let witness = match preimage {
        Some(preimage) => Witness::from_slice(&[
            encoded_signature(&signature),
            preimage.to_vec(),
            vec![1],
            witness_script.into_bytes(),
        ]),
        None => Witness::from_slice(&[
            encoded_signature(&signature),
            Vec::new(),
            witness_script.into_bytes(),
        ]),
    };
    transaction.input[0].witness = witness;

    signed_transaction(transaction)
}

/// Extracts and verifies a claim preimage from a signed Bitcoin witness.
pub fn extract_claim_preimage(
    raw_hex: &str,
    contract: &HtlcContract,
    input_index: usize,
) -> Result<[u8; 32], ChainError> {
    validate_bitcoin_contract(contract)?;
    let transaction: Transaction = bitcoin::consensus::encode::deserialize_hex(raw_hex)
        .map_err(|error| ChainError::Serialization(error.to_string()))?;
    let witness = transaction
        .input
        .get(input_index)
        .ok_or_else(|| ChainError::Validation("claim input index is out of range".to_owned()))?
        .witness
        .to_vec();

    if witness.len() != 4
        || witness[2] != [1]
        || witness[3] != expected_witness_script(contract)?.into_bytes()
    {
        return Err(ChainError::Validation(
            "claim witness does not match the HTLC branch".to_owned(),
        ));
    }

    let bytes: [u8; 32] = witness[1].as_slice().try_into().map_err(|_| {
        ChainError::Validation("claim witness preimage must contain 32 bytes".to_owned())
    })?;
    validate_preimage(contract, bytes)?;

    Ok(bytes)
}

fn validate_bitcoin_contract(contract: &HtlcContract) -> Result<(), ChainError> {
    validate_htlc_contract(contract)?;

    if contract.chain != Chain::BitcoinRegtest || contract.asset_id != AssetId::Bitcoin {
        return Err(ChainError::WrongNetwork(
            "Bitcoin adapter requires bitcoin-regtest and native BTC".to_owned(),
        ));
    }

    Ok(())
}

fn validate_funding_amounts(args: &BitcoinFundingArgs) -> Result<(), ChainError> {
    let required = args
        .htlc_amount_sats
        .checked_add(args.change_amount_sats)
        .and_then(|value| value.checked_add(args.fee_sats))
        .ok_or_else(|| ChainError::InvalidInput("Bitcoin funding amount overflow".to_owned()))?;

    if args.fee_sats == 0 || args.input.amount_sats != required {
        return Err(ChainError::InvalidInput(
            "funding input must equal HTLC amount plus change plus positive fee".to_owned(),
        ));
    }

    Ok(())
}

fn funding_outputs(args: &BitcoinFundingArgs, htlc_script: ScriptBuf) -> Vec<TxOut> {
    let htlc_output = TxOut {
        value: Amount::from_sat(args.htlc_amount_sats),
        script_pubkey: htlc_script,
    };

    match args.change_amount_sats {
        0 => vec![htlc_output],
        amount => vec![
            htlc_output,
            TxOut {
                value: Amount::from_sat(amount),
                script_pubkey: args.change_script.clone(),
            },
        ],
    }
}

fn spend_lock_time(args: &BitcoinSpendArgs) -> Result<LockTime, ChainError> {
    match args.kind {
        SpendKind::Claim => Ok(LockTime::ZERO),
        SpendKind::Refund => u32::try_from(args.contract.refund_lock_height)
            .map(LockTime::from_consensus)
            .map_err(|_| ChainError::InvalidInput("refund lock height exceeds u32".to_owned())),
    }
}

fn spend_sequence(kind: SpendKind) -> Sequence {
    match kind {
        SpendKind::Claim => Sequence::from_consensus(0xffff_fffd),
        SpendKind::Refund => Sequence::from_consensus(0xffff_fffe),
    }
}

fn parse_script(value: &str, label: &str) -> Result<ScriptBuf, ChainError> {
    let bytes = hex::decode(value)
        .map_err(|_| ChainError::InvalidInput(format!("{label} is not hexadecimal")))?;

    Ok(ScriptBuf::from_bytes(bytes))
}

fn parse_txid(value: &str) -> Result<Txid, ChainError> {
    Txid::from_str(value)
        .map_err(|_| ChainError::InvalidInput("funding txid is malformed".to_owned()))
}

fn expected_witness_script(contract: &HtlcContract) -> Result<ScriptBuf, ChainError> {
    build_htlc_witness_script(
        contract.hash_lock,
        &contract.claim_pubkey,
        &contract.refund_pubkey,
        contract.refund_lock_height,
    )
}

fn expected_output_script(contract: &HtlcContract) -> Result<ScriptBuf, ChainError> {
    Ok(expected_witness_script(contract)?.to_p2wsh())
}

fn validate_preimage(contract: &HtlcContract, preimage: [u8; 32]) -> Result<(), ChainError> {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(preimage);

    if digest.as_slice() != contract.hash_lock {
        return Err(ChainError::Validation(
            "preimage does not match the HTLC hash commitment".to_owned(),
        ));
    }

    Ok(())
}

fn sign_sighash(digest: [u8; 32], private_key: &SecretKey) -> Result<Signature, ChainError> {
    let secp = Secp256k1::new();
    let message = Message::from_digest(digest);

    Ok(secp.sign_ecdsa(&message, private_key))
}

fn encoded_signature(signature: &Signature) -> Vec<u8> {
    let mut encoded = signature.serialize_der().to_vec();
    encoded.push(EcdsaSighashType::All.to_u32() as u8);

    encoded
}

fn signed_transaction(transaction: Transaction) -> Result<SignedTransaction, ChainError> {
    if transaction
        .input
        .iter()
        .any(|input| input.witness.is_empty())
    {
        return Err(ChainError::Validation(
            "transaction contains an unsigned input".to_owned(),
        ));
    }

    Ok(SignedTransaction {
        raw_hex: serialize_hex(&transaction),
        txid: transaction.compute_txid().to_string(),
    })
}
