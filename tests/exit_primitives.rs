use bitcoin::absolute::LockTime;
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::hashes::Hash;
use bitcoin::opcodes::all::{OP_EQUAL, OP_HASH160};
use bitcoin::script::Builder;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use bitcoin::transaction::Version;
use bitcoin::{Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness};
use cross_network_market_maker::exit::{
    IdentifyCsvSpendArgs, IdentifyTimeoutArgs, IdentifyTimeoutByHashArgs, account_exit_recovery,
    identify_csv_spend, identify_htlc_timeout, identify_htlc_timeout_by_hash,
    parse_pending_force_closes, validate_csv_sequence,
};
use ripemd::{Digest, Ripemd160};
use serde_json::json;

fn destination_script() -> ScriptBuf {
    let key = SecretKey::from_slice(&[31; 32]).expect("test key");
    let public = bitcoin::PublicKey::new(PublicKey::from_secret_key(&Secp256k1::new(), &key));
    let compressed = bitcoin::key::CompressedPublicKey::try_from(public).expect("compressed key");

    ScriptBuf::new_p2wpkh(&compressed.wpubkey_hash())
}

#[test]
fn timeout_proof_binds_hash_and_paired_output() {
    let commitment_txid = Txid::from_byte_array([17; 32]);
    let payment_hash = [41; 32];
    let paired_script = destination_script();
    let payment_hash160: [u8; 20] = Ripemd160::digest(payment_hash).into();
    let witness_script = Builder::new()
        .push_opcode(OP_HASH160)
        .push_slice(payment_hash160)
        .push_opcode(OP_EQUAL)
        .into_script();
    let commitment_script = ScriptBuf::new_p2wsh(&witness_script.wscript_hash());
    let first_signature = der_signature(1);
    let second_signature = der_signature(2);
    let timeout = Transaction {
        version: Version::TWO,
        lock_time: LockTime::from_consensus(685),
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: commitment_txid,
                vout: 5,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::from_consensus(1),
            witness: Witness::from_slice(&[
                Vec::new(),
                first_signature,
                second_signature,
                Vec::new(),
                witness_script.as_bytes().to_vec(),
            ]),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(23_456),
            script_pubkey: paired_script.clone(),
        }],
    };
    let proof = identify_htlc_timeout(&IdentifyTimeoutArgs {
        raw_hex: &serialize_hex(&timeout),
        commitment_txid: &commitment_txid.to_string(),
        payment_hash,
        candidate_vouts: &[5],
        expected_commitment_script: &commitment_script,
        expected_output_script: &paired_script,
        expected_amount_sats: 23_456,
    })
    .expect("timeout proof");

    assert_eq!(proof.commitment_vout, 5);
    assert_eq!(proof.timeout_output_index, 0);
    assert_eq!(proof.lock_time, 685);

    let candidate_outputs = vec![(5, commitment_script)];
    let learned = identify_htlc_timeout_by_hash(&IdentifyTimeoutByHashArgs {
        raw_hex: &serialize_hex(&timeout),
        commitment_txid: &commitment_txid.to_string(),
        payment_hash,
        candidate_outputs: &candidate_outputs,
        expected_amount_sats: 23_456,
    })
    .expect("learn timeout paired script");

    assert_eq!(learned.timeout_output_script, paired_script);
}

fn der_signature(seed: u8) -> Vec<u8> {
    let mut signature = vec![0x30, 0x44, 0x02, 0x20];

    signature.extend([seed; 32]);
    signature.extend([0x02, 0x20]);
    signature.extend([seed.saturating_add(1); 32]);
    signature.push(0x83);

    signature
}

#[test]
fn csv_proof_rejects_rbf_disable_bit_and_requires_maturity() {
    let source_txid = Txid::from_byte_array([42; 32]);
    let script = destination_script();
    let sweep = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: source_txid,
                vout: 1,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::from_consensus(144),
            witness: Witness::default(),
        }],
        output: vec![TxOut {
            value: Amount::from_sat(22_000),
            script_pubkey: script.clone(),
        }],
    };
    let proof = identify_csv_spend(&IdentifyCsvSpendArgs {
        raw_hex: &serialize_hex(&sweep),
        source_txid: &source_txid.to_string(),
        source_vout: 1,
        required_csv: 144,
        source_height: 686,
        sweep_height: 830,
        owned_scripts: std::slice::from_ref(&script),
    })
    .expect("CSV proof");
    assert_eq!(proof.recovered_sats, 22_000);

    assert!(validate_csv_sequence(0xffff_fffd, 144).is_err());

    let foreign_sweep = Transaction {
        output: vec![
            TxOut {
                value: Amount::from_sat(22_000),
                script_pubkey: script.clone(),
            },
            TxOut {
                value: Amount::from_sat(1),
                script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
            },
        ],
        ..sweep
    };
    assert!(
        identify_csv_spend(&IdentifyCsvSpendArgs {
            raw_hex: &serialize_hex(&foreign_sweep),
            source_txid: &source_txid.to_string(),
            source_vout: 1,
            required_csv: 144,
            source_height: 686,
            sweep_height: 830,
            owned_scripts: &[script],
        })
        .is_err()
    );
}

#[test]
fn pending_parser_preserves_negative_maturity() {
    let txid = "11".repeat(32);
    let payload = json!({
        "pending_force_closing_channels": [{
            "channel": {"channel_point": format!("{txid}:0")},
            "closing_txid": "22".repeat(32),
            "limbo_balance": "23456",
            "maturity_height": "787",
            "blocks_til_maturity": "-43",
            "pending_htlcs": [{
                "outpoint": format!("{txid}:1"),
                "amount": "23456",
                "incoming": false,
                "maturity_height": "830",
                "blocks_til_maturity": "-1",
                "stage": "2"
            }]
        }]
    });
    let parsed = parse_pending_force_closes(&payload).expect("pending force close");

    assert_eq!(parsed[0].blocks_til_maturity, -43);
    assert_eq!(parsed[0].pending_htlcs[0].stage, 2);
}

#[test]
fn recovery_accounting_assigns_both_stage_fees() {
    let accounting =
        account_exit_recovery(23_456, 23_456, 121, 23_321, 135).expect("exit accounting");

    assert_eq!(accounting.net_recovered_sats, 23_200);

    let sponsored =
        account_exit_recovery(23_456, 23_456, 121, 24_321, 135).expect("sponsor accounting");

    assert_eq!(sponsored.attributable_final_sweep_sats, 23_456);
    assert_eq!(sponsored.net_recovered_sats, 23_200);
}
