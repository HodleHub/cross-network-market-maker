use bitcoin::ScriptBuf;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use cross_network_market_maker::chain::{
    AssetId, BitcoinFundingArgs, BitcoinSpendArgs, BitcoinUtxo, Chain, LiquidFundingArgs,
    LiquidSpendArgs, LiquidUtxo, SpendKind, build_bitcoin_funding_transaction,
    build_bitcoin_htlc_spend, build_liquid_funding_transaction, build_liquid_htlc_spend,
    claim_liquid_htlc, create_htlc_contract, extract_claim_preimage, extract_liquid_claim_preimage,
    sign_bitcoin_funding_transaction, sign_bitcoin_htlc_spend, sign_liquid_funding_transaction,
    sign_liquid_htlc_spend,
};
use sha2::{Digest, Sha256};
use std::str::FromStr;

fn secret(value: u8) -> SecretKey {
    SecretKey::from_slice(&[value; 32]).expect("test key is valid")
}

fn public_key(key: &SecretKey) -> PublicKey {
    PublicKey::from_secret_key(&Secp256k1::new(), key)
}

fn p2wpkh_script(key: &SecretKey) -> ScriptBuf {
    let public_key = bitcoin::PublicKey::new(public_key(key));
    let compressed = bitcoin::key::CompressedPublicKey::try_from(public_key)
        .expect("test public key is compressed");

    ScriptBuf::new_p2wpkh(&compressed.wpubkey_hash())
}

fn preimage() -> [u8; 32] {
    [0xabu8; 32]
}

fn hash_lock() -> [u8; 32] {
    Sha256::digest(preimage()).into()
}

#[test]
fn bitcoin_claim_witness_is_signed_and_recoverable() {
    let claim_key = secret(3);
    let refund_key = secret(4);
    let wallet_key = secret(5);
    let contract = create_htlc_contract(
        Chain::BitcoinRegtest,
        AssetId::Bitcoin,
        hash_lock(),
        public_key(&claim_key),
        public_key(&refund_key),
        500,
    )
    .expect("Bitcoin contract");
    let input = BitcoinUtxo {
        txid: "11".repeat(32),
        vout: 0,
        amount_sats: 7_000,
        script_pubkey: p2wpkh_script(&wallet_key),
        confirmations: 1,
    };
    let funding = build_bitcoin_funding_transaction(&BitcoinFundingArgs {
        input: input.clone(),
        contract: contract.clone(),
        htlc_amount_sats: 5_000,
        change_script: p2wpkh_script(&wallet_key),
        change_amount_sats: 1_800,
        fee_sats: 200,
    })
    .expect("Bitcoin funding transaction");
    let signed_funding = sign_bitcoin_funding_transaction(funding, &input, &wallet_key)
        .expect("Bitcoin funding signature");
    let funding_txid = bitcoin::Txid::from_str(&signed_funding.txid).expect("funding txid");
    let spend = build_bitcoin_htlc_spend(&BitcoinSpendArgs {
        contract: contract.clone(),
        funding_txid,
        funding_vout: 0,
        funding_amount_sats: 5_000,
        destination_script: p2wpkh_script(&claim_key),
        fee_sats: 100,
        kind: SpendKind::Claim,
    })
    .expect("Bitcoin claim transaction");
    let signed_claim =
        sign_bitcoin_htlc_spend(spend, &contract, &claim_key, 5_000, Some(preimage()))
            .expect("Bitcoin claim signature");

    assert_eq!(
        extract_claim_preimage(&signed_claim.raw_hex, &contract, 0).expect("claim preimage"),
        preimage()
    );
}

#[test]
fn liquid_claim_has_explicit_asset_and_separate_fee_output() {
    let claim_key = secret(7);
    let refund_key = secret(8);
    let payment_key = secret(9);
    let fee_key = secret(10);
    let payment_asset = AssetId::Explicit([0x44; 32]);
    let fee_asset = AssetId::Explicit([0x55; 32]);
    let contract = create_htlc_contract(
        Chain::LiquidRegtest,
        payment_asset,
        hash_lock(),
        public_key(&claim_key),
        public_key(&refund_key),
        500,
    )
    .expect("Liquid contract");
    assert!(contract.address.starts_with("ert1"));
    let payment_input = LiquidUtxo {
        txid: "22".repeat(32),
        vout: 1,
        asset_id: payment_asset,
        amount_sats: 6_000,
        script_pubkey: p2wpkh_script(&payment_key),
        confirmations: 1,
    };
    let fee_input = LiquidUtxo {
        txid: "33".repeat(32),
        vout: 2,
        asset_id: fee_asset,
        amount_sats: 300,
        script_pubkey: p2wpkh_script(&fee_key),
        confirmations: 1,
    };
    let funding = build_liquid_funding_transaction(&LiquidFundingArgs {
        payment_input: payment_input.clone(),
        fee_input: fee_input.clone(),
        contract: contract.clone(),
        htlc_amount_sats: 5_000,
        payment_change_script: p2wpkh_script(&payment_key),
        payment_change_amount_sats: 1_000,
        fee_change_script: p2wpkh_script(&fee_key),
        fee_change_amount_sats: 200,
        fee_sats: 100,
        fee_asset_id: fee_asset,
    })
    .expect("Liquid funding transaction");
    assert_eq!(funding.output.len(), 4);
    assert_eq!(funding.output[0].asset.to_string(), payment_asset.to_hex());
    assert!(funding.output[3].is_fee());
    let signed_funding = sign_liquid_funding_transaction(
        funding,
        &payment_input,
        &fee_input,
        &payment_key,
        &fee_key,
    )
    .expect("Liquid funding signature");
    let funding_txid = elements::Txid::from_str(&signed_funding.txid).expect("Liquid txid");
    let spend = claim_liquid_htlc(&LiquidSpendArgs {
        contract: contract.clone(),
        funding_txid,
        funding_vout: 0,
        funding_amount_sats: 5_000,
        fee_input: LiquidUtxo {
            txid: "44".repeat(32),
            vout: 0,
            asset_id: fee_asset,
            amount_sats: 300,
            script_pubkey: p2wpkh_script(&fee_key),
            confirmations: 1,
        },
        destination_script: p2wpkh_script(&claim_key),
        fee_change_script: p2wpkh_script(&fee_key),
        fee_change_amount_sats: 200,
        fee_sats: 100,
        fee_asset_id: fee_asset,
        kind: SpendKind::Claim,
    })
    .expect("Liquid claim transaction");
    let signed_claim = sign_liquid_htlc_spend(
        spend,
        &contract,
        &LiquidUtxo {
            txid: "44".repeat(32),
            vout: 0,
            asset_id: fee_asset,
            amount_sats: 300,
            script_pubkey: p2wpkh_script(&fee_key),
            confirmations: 1,
        },
        &claim_key,
        &fee_key,
        5_000,
        Some(preimage()),
    )
    .expect("Liquid claim signature");

    assert_eq!(
        extract_liquid_claim_preimage(&signed_claim.raw_hex, &contract, 0)
            .expect("Liquid claim preimage"),
        preimage()
    );
}

#[test]
fn liquid_wrong_asset_and_wrong_preimage_are_rejected() {
    let claim_key = secret(12);
    let refund_key = secret(13);
    let contract = create_htlc_contract(
        Chain::LiquidRegtest,
        AssetId::Explicit([0x66; 32]),
        hash_lock(),
        public_key(&claim_key),
        public_key(&refund_key),
        500,
    )
    .expect("Liquid contract");
    let result = build_liquid_funding_transaction(&LiquidFundingArgs {
        payment_input: LiquidUtxo {
            txid: "55".repeat(32),
            vout: 0,
            asset_id: AssetId::Explicit([0x67; 32]),
            amount_sats: 6_000,
            script_pubkey: p2wpkh_script(&claim_key),
            confirmations: 1,
        },
        fee_input: LiquidUtxo {
            txid: "56".repeat(32),
            vout: 0,
            asset_id: AssetId::Explicit([0x77; 32]),
            amount_sats: 300,
            script_pubkey: p2wpkh_script(&refund_key),
            confirmations: 1,
        },
        contract,
        htlc_amount_sats: 5_000,
        payment_change_script: p2wpkh_script(&claim_key),
        payment_change_amount_sats: 1_000,
        fee_change_script: p2wpkh_script(&refund_key),
        fee_change_amount_sats: 200,
        fee_sats: 100,
        fee_asset_id: AssetId::Explicit([0x77; 32]),
    });

    assert!(result.is_err());
}

#[test]
fn liquid_refund_builder_uses_absolute_lock_height() {
    let claim_key = secret(14);
    let refund_key = secret(15);
    let fee_key = secret(16);
    let fee_asset = AssetId::Explicit([0x88; 32]);
    let contract = create_htlc_contract(
        Chain::LiquidRegtest,
        AssetId::Explicit([0x99; 32]),
        hash_lock(),
        public_key(&claim_key),
        public_key(&refund_key),
        700,
    )
    .expect("Liquid contract");
    let refund = build_liquid_htlc_spend(&LiquidSpendArgs {
        contract,
        funding_txid: elements::Txid::from_str(&"aa".repeat(32)).expect("funding txid"),
        funding_vout: 0,
        funding_amount_sats: 5_000,
        fee_input: LiquidUtxo {
            txid: "bb".repeat(32),
            vout: 0,
            asset_id: fee_asset,
            amount_sats: 300,
            script_pubkey: p2wpkh_script(&fee_key),
            confirmations: 1,
        },
        destination_script: p2wpkh_script(&refund_key),
        fee_change_script: p2wpkh_script(&fee_key),
        fee_change_amount_sats: 200,
        fee_sats: 100,
        fee_asset_id: fee_asset,
        kind: SpendKind::Refund,
    })
    .expect("Liquid refund transaction");

    assert_eq!(refund.lock_time.to_consensus_u32(), 700);
    assert_eq!(refund.input[0].sequence.0, 0xffff_fffe);
}
