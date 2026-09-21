//! On-chain proof helpers for batched HTLC timeout and CSV sweep transactions.

use std::str::FromStr;

use bitcoin::consensus::encode::deserialize;
use bitcoin::opcodes::all::{OP_EQUAL, OP_EQUALVERIFY, OP_HASH160};
use bitcoin::script::Instruction;
use bitcoin::{ScriptBuf, Transaction, Txid};
use ripemd::{Digest, Ripemd160};

use super::error::ExitError;
use super::types::{CsvSpendProof, ExitRecoveryAccounting, HtlcTimeoutProof, OwnedOutputSet};

/// Arguments for identifying one offered HTLC timeout in a batched transaction.
pub struct IdentifyTimeoutArgs<'a> {
    /// Raw confirmed timeout transaction bytes.
    pub raw_hex: &'a str,
    /// Commitment transaction identifier being spent.
    pub commitment_txid: &'a str,
    /// Payment hash commitment used by the target HTLC.
    pub payment_hash: [u8; 32],
    /// Candidate commitment output indices from the close transaction.
    pub candidate_vouts: &'a [u32],
    /// P2WSH script committed by the candidate close output.
    pub expected_commitment_script: &'a ScriptBuf,
    /// Expected paired SIGHASH_SINGLE output script.
    pub expected_output_script: &'a ScriptBuf,
    /// Expected target HTLC amount.
    pub expected_amount_sats: u64,
}

/// Arguments for proving a target timeout when the paired script is learned on chain.
pub struct IdentifyTimeoutByHashArgs<'a> {
    /// Raw confirmed timeout transaction bytes.
    pub raw_hex: &'a str,
    /// Commitment transaction identifier being spent.
    pub commitment_txid: &'a str,
    /// Payment hash commitment used by the target HTLC.
    pub payment_hash: [u8; 32],
    /// Candidate close outputs and their exact P2WSH scripts.
    pub candidate_outputs: &'a [(u32, ScriptBuf)],
    /// Expected target HTLC amount in the paired timeout output.
    pub expected_amount_sats: u64,
}

/// Arguments for proving one delayed CSV sweep.
pub struct IdentifyCsvSpendArgs<'a> {
    /// Raw confirmed CSV sweep transaction bytes.
    pub raw_hex: &'a str,
    /// Delayed source transaction identifier.
    pub source_txid: &'a str,
    /// Delayed source output index.
    pub source_vout: u32,
    /// Required BIP68 block delay.
    pub required_csv: u16,
    /// Confirmation height of the delayed source transaction.
    pub source_height: u64,
    /// Confirmation height of the sweep transaction.
    pub sweep_height: u64,
    /// Payer wallet scripts whose output value may be counted.
    pub owned_scripts: &'a [ScriptBuf],
}

/// Identifies the exact target input and paired output in a batched timeout.
pub fn identify_htlc_timeout(
    args: &IdentifyTimeoutArgs<'_>,
) -> Result<HtlcTimeoutProof, ExitError> {
    let transaction = parse_transaction(args.raw_hex)?;
    let commitment_txid = parse_commitment_txid(args.commitment_txid)?;
    let candidate_script = args.expected_commitment_script;
    let candidate_outputs = args
        .candidate_vouts
        .iter()
        .map(|vout| (*vout, candidate_script.clone()))
        .collect::<Vec<_>>();

    find_timeout_proof(
        &transaction,
        &commitment_txid,
        args.commitment_txid,
        args.payment_hash,
        candidate_outputs.as_slice(),
        Some(args.expected_output_script),
        args.expected_amount_sats,
    )
}

/// Proves a target timeout while learning its delayed paired script from SIGHASH_SINGLE.
pub fn identify_htlc_timeout_by_hash(
    args: &IdentifyTimeoutByHashArgs<'_>,
) -> Result<HtlcTimeoutProof, ExitError> {
    let transaction = parse_transaction(args.raw_hex)?;
    let commitment_txid = parse_commitment_txid(args.commitment_txid)?;

    find_timeout_proof(
        &transaction,
        &commitment_txid,
        args.commitment_txid,
        args.payment_hash,
        args.candidate_outputs,
        None,
        args.expected_amount_sats,
    )
}

fn find_timeout_proof(
    transaction: &Transaction,
    commitment_txid: &Txid,
    commitment_txid_text: &str,
    payment_hash: [u8; 32],
    candidate_outputs: &[(u32, ScriptBuf)],
    expected_output_script: Option<&ScriptBuf>,
    expected_amount_sats: u64,
) -> Result<HtlcTimeoutProof, ExitError> {
    let payment_hash160 = Ripemd160::digest(payment_hash);

    for (input_index, input) in transaction.input.iter().enumerate() {
        let Some((_, commitment_script)) = candidate_outputs
            .iter()
            .find(|(vout, _)| *vout == input.previous_output.vout)
        else {
            continue;
        };

        if input.previous_output.txid != *commitment_txid || input_index >= transaction.output.len()
        {
            continue;
        }

        let Ok(witness_script) = timeout_witness_script(&input.witness, &payment_hash160) else {
            continue;
        };
        let expected_p2wsh = ScriptBuf::new_p2wsh(&witness_script.wscript_hash());

        if expected_p2wsh != *commitment_script || !has_single_sighash(&input.witness) {
            continue;
        }

        let output = &transaction.output[input_index];

        if output.value.to_sat() != expected_amount_sats
            || expected_output_script.is_some_and(|script| output.script_pubkey != *script)
        {
            continue;
        }

        return Ok(HtlcTimeoutProof {
            timeout_txid: transaction.compute_txid().to_string(),
            commitment_txid: commitment_txid_text.to_owned(),
            commitment_vout: input.previous_output.vout,
            timeout_input_index: input_index,
            timeout_output_index: input_index,
            timeout_amount_sats: output.value.to_sat(),
            timeout_output_script: output.script_pubkey.clone(),
            lock_time: transaction.lock_time.to_consensus_u32(),
            sequence: input.sequence.to_consensus_u32(),
        });
    }

    Err(ExitError::Proof(
        "timeout transaction does not bind the target HTLC hash, HTLC script, and paired output"
            .to_owned(),
    ))
}

fn parse_commitment_txid(value: &str) -> Result<Txid, ExitError> {
    Txid::from_str(value)
        .map_err(|_| ExitError::InvalidData("commitment txid is invalid".to_owned()))
}

fn timeout_witness_script(
    witness: &bitcoin::Witness,
    payment_hash160: &[u8],
) -> Result<ScriptBuf, ExitError> {
    let script_bytes = witness
        .last()
        .ok_or_else(|| ExitError::Proof("timeout witness lacks a redeem script".to_owned()))?;
    let script = ScriptBuf::from_bytes(script_bytes.to_vec());
    let instructions = script
        .instructions()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ExitError::Proof("timeout witness redeem script is malformed".to_owned()))?;
    let has_payment_hash = instructions.windows(3).any(|window| {
        matches!(window[0], Instruction::Op(op) if op == OP_HASH160)
            && matches!(window[1], Instruction::PushBytes(bytes) if bytes.as_bytes() == payment_hash160)
            && matches!(window[2], Instruction::Op(op) if op == OP_EQUAL || op == OP_EQUALVERIFY)
    });

    if !has_payment_hash {
        return Err(ExitError::Proof(
            "timeout witness redeem script lacks the target payment hash".to_owned(),
        ));
    }

    Ok(script)
}

fn has_single_sighash(witness: &bitcoin::Witness) -> bool {
    witness.len() == 5
        && witness.iter().next().is_some_and(|item| item.is_empty())
        && witness.iter().nth(3).is_some_and(|item| item.is_empty())
        && witness.iter().skip(1).take(2).all(|signature| {
            bitcoin::ecdsa::Signature::from_slice(signature).is_ok_and(|parsed| {
                parsed.sighash_type == bitcoin::EcdsaSighashType::SinglePlusAnyoneCanPay
            })
        })
}

/// Proves a delayed source output was spent with a valid block based CSV sequence.
pub fn identify_csv_spend(args: &IdentifyCsvSpendArgs<'_>) -> Result<CsvSpendProof, ExitError> {
    let transaction = parse_transaction(args.raw_hex)?;
    let source_txid = Txid::from_str(args.source_txid)
        .map_err(|_| ExitError::InvalidData("CSV source txid is invalid".to_owned()))?;
    let (sweep_input_index, input) = transaction
        .input
        .iter()
        .enumerate()
        .find(|(_, input)| {
            input.previous_output.txid == source_txid
                && input.previous_output.vout == args.source_vout
        })
        .ok_or_else(|| {
            ExitError::Proof("CSV sweep does not spend the expected source output".to_owned())
        })?;
    validate_csv_sequence(input.sequence.to_consensus_u32(), args.required_csv)?;

    if args.sweep_height < args.source_height
        || args.sweep_height - args.source_height < u64::from(args.required_csv)
    {
        return Err(ExitError::Proof(
            "CSV sweep confirmation height is before the required maturity".to_owned(),
        ));
    }

    let owned_outputs = verify_owned_outputs(&transaction, args.owned_scripts)?;

    Ok(CsvSpendProof {
        sweep_txid: transaction.compute_txid().to_string(),
        source_txid: args.source_txid.to_owned(),
        source_vout: args.source_vout,
        sweep_input_index,
        sequence: input.sequence.to_consensus_u32(),
        required_csv: args.required_csv,
        source_height: args.source_height,
        sweep_height: args.sweep_height,
        recovered_sats: owned_outputs.amount_sats,
    })
}

/// Validates BIP68 block mode and the required relative sequence.
pub fn validate_csv_sequence(sequence: u32, required_csv: u16) -> Result<(), ExitError> {
    if sequence & 0x8000_0000 != 0
        || sequence & 0x0040_0000 != 0
        || sequence & 0x0000_ffff < u32::from(required_csv)
    {
        return Err(ExitError::Proof(
            "CSV sequence disables relative blocks, selects time mode, or is too short".to_owned(),
        ));
    }

    Ok(())
}

/// Verifies final output ownership without choosing a largest or sponsor-change output.
pub fn verify_owned_outputs(
    transaction: &Transaction,
    owned_scripts: &[ScriptBuf],
) -> Result<OwnedOutputSet, ExitError> {
    if transaction.output.iter().any(|output| {
        output.value.to_sat() > 0
            && !owned_scripts
                .iter()
                .any(|script| script == &output.script_pubkey)
    }) {
        return Err(ExitError::Proof(
            "transaction has a positive output outside the payer wallet".to_owned(),
        ));
    }

    let scripts = transaction
        .output
        .iter()
        .filter(|output| {
            owned_scripts
                .iter()
                .any(|script| script == &output.script_pubkey)
        })
        .map(|output| output.script_pubkey.clone())
        .collect::<Vec<_>>();
    let amount_sats = transaction
        .output
        .iter()
        .filter(|output| {
            owned_scripts
                .iter()
                .any(|script| script == &output.script_pubkey)
        })
        .map(|output| output.value.to_sat())
        .try_fold(0u64, |total, amount| {
            total
                .checked_add(amount)
                .ok_or_else(|| ExitError::Proof("owned output amount overflow".to_owned()))
        })?;

    if scripts.is_empty() || amount_sats == 0 {
        return Err(ExitError::Proof(
            "no positive owned output was observed".to_owned(),
        ));
    }

    Ok(OwnedOutputSet {
        scripts,
        amount_sats,
    })
}

/// Computes conservative recovery after assigning both timeout and sweep fees.
pub fn account_exit_recovery(
    principal_sats: u64,
    timeout_output_sats: u64,
    timeout_stage_fee_sats: u64,
    final_sweep_sats: u64,
    final_sweep_fee_sats: u64,
) -> Result<ExitRecoveryAccounting, ExitError> {
    if timeout_output_sats > principal_sats {
        return Err(ExitError::Proof(
            "exit stage output exceeds the original HTLC principal".to_owned(),
        ));
    }

    let gross_final_sweep_sats = final_sweep_sats
        .checked_add(final_sweep_fee_sats)
        .ok_or_else(|| ExitError::Proof("exit final amount overflow".to_owned()))?;
    let attributable_final_sweep_sats = gross_final_sweep_sats.min(principal_sats);
    let deductions = timeout_stage_fee_sats
        .checked_add(final_sweep_fee_sats)
        .ok_or_else(|| ExitError::Proof("exit fee accounting overflow".to_owned()))?;
    let net_recovered_sats = attributable_final_sweep_sats.saturating_sub(deductions);

    Ok(ExitRecoveryAccounting {
        principal_sats,
        timeout_output_sats,
        timeout_stage_fee_sats,
        final_sweep_sats,
        final_sweep_fee_sats,
        attributable_final_sweep_sats,
        net_recovered_sats,
    })
}

/// Parses a raw Bitcoin transaction without exposing private witness values in errors.
pub fn parse_exit_transaction(raw_hex: &str) -> Result<Transaction, ExitError> {
    parse_transaction(raw_hex)
}

fn parse_transaction(raw_hex: &str) -> Result<Transaction, ExitError> {
    let bytes = hex::decode(raw_hex)
        .map_err(|_| ExitError::Transaction("raw transaction is not hexadecimal".to_owned()))?;

    deserialize(&bytes).map_err(|error| ExitError::Transaction(error.to_string()))
}
