#![allow(clippy::too_many_arguments)]

use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bitcoin::ScriptBuf;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use bitcoin::{Address, Network};
use cross_network_market_maker::chain::{
    AssetId, BitcoinFundingArgs, BitcoinSpendArgs, BitcoinUtxo, BroadcastBitcoinArgs, Chain,
    InspectBitcoinHtlcArgs, LiquidFundingArgs, LiquidSpendArgs, LiquidUtxo,
    ObserveBitcoinClaimArgs, ObserveLiquidClaimArgs, SpendKind, broadcast_bitcoin_transaction,
    broadcast_liquid_transaction, build_bitcoin_funding_transaction, build_bitcoin_htlc_spend,
    build_liquid_funding_transaction, build_liquid_htlc_spend, create_htlc_contract,
    inspect_bitcoin_htlc_output, inspect_liquid_htlc_output, observe_bitcoin_claim,
    observe_liquid_claim, parse_rpc_coin_amount, sign_bitcoin_funding_transaction,
    sign_bitcoin_htlc_spend, sign_liquid_funding_transaction, sign_liquid_htlc_spend,
};
use cross_network_market_maker::rpc::RpcClient;
use rand::rngs::OsRng;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const BITCOIN_RPC_URL: &str = "http://127.0.0.1:29443";
const ELEMENTS_RPC_URL: &str = "http://127.0.0.1:27051";
const RPC_USER: &str = "cross_network_market_maker";
const BITCOIN_RPC_PASSWORD: &str = "cross_network_market_maker_rpc_password";
const ELEMENTS_RPC_PASSWORD: &str = "cross_network_market_maker_elements_rpc_password";
const MIN_CONFIRMATIONS: u32 = 1;

#[derive(Clone, Debug, Deserialize)]
struct ChainInfo {
    chain: String,
    blocks: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct BitcoinRawOutput {
    n: u32,
    value: Value,
    #[serde(rename = "scriptPubKey")]
    script_pub_key: BitcoinRawScript,
}

#[derive(Clone, Debug, Deserialize)]
struct BitcoinRawScript {
    hex: String,
}

#[derive(Clone, Debug, Deserialize)]
struct BitcoinRawTransaction {
    vout: Vec<BitcoinRawOutput>,
}

#[derive(Clone, Debug, Deserialize)]
struct LiquidUnspent {
    txid: String,
    vout: u32,
    asset: String,
    #[serde(default)]
    amount: Option<Value>,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    amountcommitment: Option<String>,
    #[serde(default)]
    assetcommitment: Option<String>,
    confirmations: u32,
    #[serde(rename = "scriptPubKey")]
    script_pub_key: Value,
    #[serde(default)]
    address: Option<String>,
}

#[test]
#[ignore = "requires isolated regtest"]
fn bitcoin_regtest_claim_and_refund_use_real_core() {
    require_live_flag();
    let rpc = bitcoin_rpc(
        BITCOIN_RPC_URL,
        "XMM_BITCOIN_RPC_PASSWORD",
        BITCOIN_RPC_PASSWORD,
    );
    let nonce = unique_nonce();
    let miner_name = format!("xmm-miner-{nonce}");
    let miner = wallet_rpc(
        BITCOIN_RPC_URL,
        &miner_name,
        "XMM_BITCOIN_RPC_PASSWORD",
        BITCOIN_RPC_PASSWORD,
    );

    create_wallet(&miner, &miner_name);
    let miner_address = new_bitcoin_address(&miner);
    mine_bitcoin(&miner, &miner_address, 101);
    let wallet_key = SecretKey::new(&mut OsRng);
    let wallet_address = bitcoin_key_address(&wallet_key);
    let wallet_script = p2wpkh_script(&wallet_key);
    let source = fund_bitcoin_key(
        &rpc,
        &miner,
        &miner_address,
        &wallet_address,
        &wallet_script,
    );
    let claim_key = SecretKey::new(&mut OsRng);
    let refund_key = SecretKey::new(&mut OsRng);
    let preimage: [u8; 32] = rand::random();
    let contract = bitcoin_contract(&rpc, &claim_key, &refund_key, preimage, 30);
    let funding = build_bitcoin_funding_transaction(&BitcoinFundingArgs {
        input: source.clone(),
        contract: contract.clone(),
        htlc_amount_sats: 50_000,
        change_script: source.script_pubkey.clone(),
        change_amount_sats: source.amount_sats - 50_000 - 1_000,
        fee_sats: 1_000,
    })
    .expect("build Bitcoin funding");
    let signed_funding = sign_bitcoin_funding_transaction(funding, &source, &wallet_key)
        .expect("sign Bitcoin funding");
    let funding_txid = broadcast_bitcoin_transaction(&BroadcastBitcoinArgs {
        rpc: &rpc,
        raw_hex: &signed_funding.raw_hex,
    })
    .expect("broadcast Bitcoin funding")
    .txid;
    mine_bitcoin(&miner, &miner_address, 1);
    let funding_evidence = inspect_bitcoin_htlc_output(&InspectBitcoinHtlcArgs {
        rpc: &rpc,
        txid: &funding_txid,
        vout: 0,
        contract: &contract,
        expected_amount_sats: 50_000,
        min_confirmations: MIN_CONFIRMATIONS,
        min_headroom_blocks: 2,
    })
    .expect("inspect Bitcoin funding");

    let claim_spend = build_bitcoin_htlc_spend(&BitcoinSpendArgs {
        contract: contract.clone(),
        funding_txid: funding_txid.parse().expect("funding txid"),
        funding_vout: 0,
        funding_amount_sats: 50_000,
        destination_script: source.script_pubkey.clone(),
        fee_sats: 500,
        kind: SpendKind::Claim,
    })
    .expect("build Bitcoin claim");
    let signed_claim =
        sign_bitcoin_htlc_spend(claim_spend, &contract, &claim_key, 50_000, Some(preimage))
            .expect("sign Bitcoin claim");
    let claim_txid = broadcast_bitcoin_transaction(&BroadcastBitcoinArgs {
        rpc: &rpc,
        raw_hex: &signed_claim.raw_hex,
    })
    .expect("broadcast Bitcoin claim")
    .txid;
    mine_bitcoin(&miner, &miner_address, 1);
    let claim = observe_bitcoin_claim(&ObserveBitcoinClaimArgs {
        rpc: &rpc,
        funding_txid: &funding_txid,
        funding_vout: 0,
        claim_txid: &claim_txid,
        contract: &contract,
        min_confirmations: MIN_CONFIRMATIONS,
    })
    .expect("observe Bitcoin claim");
    assert_eq!(claim.preimage.preimage, preimage);
    assert_eq!(funding_evidence.amount_sats, 50_000);

    println!("bitcoin claim funding={funding_txid} claim={claim_txid}");
    let refund_source = fund_bitcoin_key(
        &rpc,
        &miner,
        &miner_address,
        &wallet_address,
        &wallet_script,
    );
    run_bitcoin_refund(&rpc, &miner, &miner_address, &wallet_key, &refund_source);
}

#[test]
#[ignore = "requires isolated regtest"]
fn liquid_regtest_claim_and_refund_use_real_elements() {
    require_live_flag();
    let rpc = elements_rpc(
        ELEMENTS_RPC_URL,
        "XMM_ELEMENTS_RPC_PASSWORD",
        ELEMENTS_RPC_PASSWORD,
    );
    let wallet_name = "cross_network_market_maker";
    let wallet = wallet_rpc(
        ELEMENTS_RPC_URL,
        wallet_name,
        "XMM_ELEMENTS_RPC_PASSWORD",
        ELEMENTS_RPC_PASSWORD,
    );
    let chain_info: ChainInfo = rpc
        .call("getblockchaininfo", &[])
        .expect("Elements chain info");
    assert!(chain_info.chain == "elementsregtest" || chain_info.chain == "liquidregtest");
    let test_asset = AssetId::from_hex(&env::var("XMM_TEST_DEPIX_ASSET").unwrap_or_else(|_| {
        "4fa41f2929d4bf6975a55967d9da5b650b6b9bfddeae4d7b54b04394be328f7f".to_owned()
    }))
    .expect("TEST-DEPIX asset");
    let fee_asset = AssetId::from_hex(&env::var("XMM_LBTC_ASSET").unwrap_or_else(|_| {
        "b2e15d0d7a0c94e4e2ce0fe6e8691b9e451377f6e46e8045a86f7c4b5d4f0f23".to_owned()
    }))
    .expect("LBTC asset");
    prepare_liquid_explicit_outputs(&rpc, &wallet, test_asset, fee_asset, 20_000);
    let (payment_input, payment_key) =
        liquid_wallet_utxo(&wallet, &test_asset).expect("TEST-DEPIX wallet UTXO");
    let fee_inputs = liquid_wallet_utxos(&wallet, &fee_asset);
    assert!(
        fee_inputs.len() >= 2,
        "explicit LBTC preparation needs two fee UTXOs"
    );
    let (fee_input, fee_key) = liquid_with_key(&wallet, fee_inputs[0].clone());
    let (claim_fee_input, claim_fee_key) = liquid_with_key(&wallet, fee_inputs[1].clone());
    let claim_htlc_key = SecretKey::new(&mut OsRng);
    let refund_key = SecretKey::new(&mut OsRng);
    let preimage: [u8; 32] = rand::random();
    let contract = liquid_contract(&rpc, &claim_htlc_key, &refund_key, test_asset, preimage, 30);
    let htlc_amount = 1_000u64;
    let funding_fee = 1_000u64;
    let signed_funding = sign_liquid_funding_transaction(
        build_liquid_funding_transaction(&LiquidFundingArgs {
            payment_input: payment_input.clone(),
            fee_input: fee_input.clone(),
            contract: contract.clone(),
            htlc_amount_sats: htlc_amount,
            payment_change_script: payment_input.script_pubkey.clone(),
            payment_change_amount_sats: payment_input.amount_sats - htlc_amount,
            fee_change_script: fee_input.script_pubkey.clone(),
            fee_change_amount_sats: fee_input.amount_sats - funding_fee,
            fee_sats: funding_fee,
            fee_asset_id: fee_asset,
        })
        .expect("build Liquid funding"),
        &payment_input,
        &fee_input,
        &payment_key,
        &fee_key,
    )
    .expect("sign Liquid funding");
    let funding_txid =
        broadcast_liquid_transaction(&cross_network_market_maker::chain::BroadcastLiquidArgs {
            rpc: &rpc,
            raw_hex: &signed_funding.raw_hex,
        })
        .expect("broadcast Liquid funding")
        .txid;
    let wallet_address = new_liquid_address(&wallet);
    mine_elements(&rpc, &wallet_address, 1);
    let evidence =
        inspect_liquid_htlc_output(&cross_network_market_maker::chain::InspectLiquidHtlcArgs {
            rpc: &rpc,
            txid: &funding_txid,
            vout: 0,
            contract: &contract,
            expected_amount_sats: htlc_amount,
            min_confirmations: MIN_CONFIRMATIONS,
            min_headroom_blocks: 2,
        })
        .expect("inspect Liquid funding");
    let claim_spend = build_liquid_htlc_spend(&LiquidSpendArgs {
        contract: contract.clone(),
        funding_txid: funding_txid.parse().expect("Liquid funding txid"),
        funding_vout: 0,
        funding_amount_sats: htlc_amount,
        fee_input: claim_fee_input.clone(),
        destination_script: claim_fee_input.script_pubkey.clone(),
        fee_change_script: claim_fee_input.script_pubkey.clone(),
        fee_change_amount_sats: claim_fee_input.amount_sats - 500,
        fee_sats: 500,
        fee_asset_id: fee_asset,
        kind: SpendKind::Claim,
    })
    .expect("build Liquid claim");
    let signed_claim = sign_liquid_htlc_spend(
        claim_spend,
        &contract,
        &claim_fee_input,
        &claim_htlc_key,
        &claim_fee_key,
        htlc_amount,
        Some(preimage),
    )
    .expect("sign Liquid claim");
    let claim_txid =
        broadcast_liquid_transaction(&cross_network_market_maker::chain::BroadcastLiquidArgs {
            rpc: &rpc,
            raw_hex: &signed_claim.raw_hex,
        })
        .expect("broadcast Liquid claim")
        .txid;
    mine_elements(&rpc, &wallet_address, 1);
    let claim = observe_liquid_claim(&ObserveLiquidClaimArgs {
        rpc: &rpc,
        funding_txid: &funding_txid,
        funding_vout: 0,
        claim_txid: &claim_txid,
        contract: &contract,
        min_confirmations: MIN_CONFIRMATIONS,
    })
    .expect("observe Liquid claim");
    assert_eq!(claim.preimage.preimage, preimage);
    assert_eq!(evidence.asset_id, test_asset);
    println!("liquid claim funding={funding_txid} claim={claim_txid}");
    run_liquid_refund(&rpc, &wallet, test_asset, fee_asset);
    run_liquid_lbtc_payment_claim(&rpc, &wallet, fee_asset);
}

fn run_liquid_lbtc_payment_claim(rpc: &RpcClient, wallet: &RpcClient, lbtc: AssetId) {
    prepare_liquid_explicit_outputs(rpc, wallet, lbtc, lbtc, 20_000);
    let candidates = liquid_wallet_utxos(wallet, &lbtc)
        .into_iter()
        .filter(|entry| entry.amount_sats >= 2_000)
        .take(3)
        .collect::<Vec<_>>();
    assert!(
        candidates.len() >= 3,
        "LBTC payment qualification needs three distinct explicit UTXOs"
    );
    let (payment_input, payment_key) = liquid_with_key(wallet, candidates[0].clone());
    let (fee_input, fee_key) = liquid_with_key(wallet, candidates[1].clone());
    let (claim_fee_input, claim_fee_key) = liquid_with_key(wallet, candidates[2].clone());
    let claim_key = SecretKey::new(&mut OsRng);
    let refund_key = SecretKey::new(&mut OsRng);
    let preimage: [u8; 32] = rand::random();
    let contract = liquid_contract(rpc, &claim_key, &refund_key, lbtc, preimage, 30);
    let htlc_amount = 1_500u64;
    let funding_fee = 500u64;
    let signed_funding = sign_liquid_funding_transaction(
        build_liquid_funding_transaction(&LiquidFundingArgs {
            payment_input: payment_input.clone(),
            fee_input: fee_input.clone(),
            contract: contract.clone(),
            htlc_amount_sats: htlc_amount,
            payment_change_script: payment_input.script_pubkey.clone(),
            payment_change_amount_sats: payment_input.amount_sats - htlc_amount,
            fee_change_script: fee_input.script_pubkey.clone(),
            fee_change_amount_sats: fee_input.amount_sats - funding_fee,
            fee_sats: funding_fee,
            fee_asset_id: lbtc,
        })
        .expect("build LBTC payment funding"),
        &payment_input,
        &fee_input,
        &payment_key,
        &fee_key,
    )
    .expect("sign LBTC payment funding");
    let funding_txid =
        broadcast_liquid_transaction(&cross_network_market_maker::chain::BroadcastLiquidArgs {
            rpc,
            raw_hex: &signed_funding.raw_hex,
        })
        .expect("broadcast LBTC payment funding")
        .txid;
    let mining_address = new_liquid_address(wallet);
    mine_elements(rpc, &mining_address, 1);
    inspect_liquid_htlc_output(&cross_network_market_maker::chain::InspectLiquidHtlcArgs {
        rpc,
        txid: &funding_txid,
        vout: 0,
        contract: &contract,
        expected_amount_sats: htlc_amount,
        min_confirmations: MIN_CONFIRMATIONS,
        min_headroom_blocks: 2,
    })
    .expect("inspect LBTC payment funding");
    let claim_spend = build_liquid_htlc_spend(&LiquidSpendArgs {
        contract: contract.clone(),
        funding_txid: funding_txid.parse().expect("LBTC funding txid"),
        funding_vout: 0,
        funding_amount_sats: htlc_amount,
        fee_input: claim_fee_input.clone(),
        destination_script: claim_fee_input.script_pubkey.clone(),
        fee_change_script: claim_fee_input.script_pubkey.clone(),
        fee_change_amount_sats: claim_fee_input.amount_sats - 250,
        fee_sats: 250,
        fee_asset_id: lbtc,
        kind: SpendKind::Claim,
    })
    .expect("build LBTC payment claim");
    let signed_claim = sign_liquid_htlc_spend(
        claim_spend,
        &contract,
        &claim_fee_input,
        &claim_key,
        &claim_fee_key,
        htlc_amount,
        Some(preimage),
    )
    .expect("sign LBTC payment claim");
    let claim_txid =
        broadcast_liquid_transaction(&cross_network_market_maker::chain::BroadcastLiquidArgs {
            rpc,
            raw_hex: &signed_claim.raw_hex,
        })
        .expect("broadcast LBTC payment claim")
        .txid;
    mine_elements(rpc, &mining_address, 1);
    let claim = observe_liquid_claim(&ObserveLiquidClaimArgs {
        rpc,
        funding_txid: &funding_txid,
        funding_vout: 0,
        claim_txid: &claim_txid,
        contract: &contract,
        min_confirmations: MIN_CONFIRMATIONS,
    })
    .expect("observe LBTC payment claim");
    assert_eq!(claim.preimage.preimage, preimage);
    println!("liquid LBTC payment funding={funding_txid} claim={claim_txid}");
}

fn run_liquid_refund(
    rpc: &RpcClient,
    wallet: &RpcClient,
    payment_asset: AssetId,
    fee_asset: AssetId,
) {
    let payment_input = liquid_wallet_utxos(wallet, &payment_asset)
        .into_iter()
        .filter(|entry| entry.amount_sats > 3_000)
        .max_by_key(|entry| entry.amount_sats)
        .expect("Liquid payment-asset refund UTXO");
    let (payment_input, payment_key) = liquid_with_key(wallet, payment_input);
    let fee_input = liquid_wallet_utxos(wallet, &fee_asset)
        .into_iter()
        .filter(|entry| entry.amount_sats > 1_500)
        .max_by_key(|entry| entry.amount_sats)
        .expect("Liquid fee-asset refund UTXO");
    let (fee_input, fee_key) = liquid_with_key(wallet, fee_input);
    let claim_key = SecretKey::new(&mut OsRng);
    let refund_key = SecretKey::new(&mut OsRng);
    let preimage: [u8; 32] = rand::random();
    let contract = liquid_contract(rpc, &claim_key, &refund_key, payment_asset, preimage, 8);
    let htlc_amount = 2_000u64;
    let funding_fee = 1_000u64;
    let signed_funding = sign_liquid_funding_transaction(
        build_liquid_funding_transaction(&LiquidFundingArgs {
            payment_input: payment_input.clone(),
            fee_input: fee_input.clone(),
            contract: contract.clone(),
            htlc_amount_sats: htlc_amount,
            payment_change_script: payment_input.script_pubkey.clone(),
            payment_change_amount_sats: payment_input.amount_sats - htlc_amount,
            fee_change_script: fee_input.script_pubkey.clone(),
            fee_change_amount_sats: fee_input.amount_sats - funding_fee,
            fee_sats: funding_fee,
            fee_asset_id: fee_asset,
        })
        .expect("build Liquid refund funding"),
        &payment_input,
        &fee_input,
        &payment_key,
        &fee_key,
    )
    .expect("sign Liquid refund funding");
    let funding_txid =
        broadcast_liquid_transaction(&cross_network_market_maker::chain::BroadcastLiquidArgs {
            rpc,
            raw_hex: &signed_funding.raw_hex,
        })
        .expect("broadcast Liquid refund funding")
        .txid;
    let mining_address = new_liquid_address(wallet);
    mine_elements(rpc, &mining_address, 1);
    let refund_fee_candidate = liquid_wallet_utxos(wallet, &fee_asset)
        .into_iter()
        .filter(|entry| entry.amount_sats > 500)
        .max_by_key(|entry| entry.amount_sats)
        .expect("Liquid refund spend fee UTXO");
    let (refund_fee_input, refund_fee_key) = liquid_with_key(wallet, refund_fee_candidate);
    let unsigned_refund = build_liquid_htlc_spend(&LiquidSpendArgs {
        contract: contract.clone(),
        funding_txid: funding_txid.parse().expect("Liquid refund funding txid"),
        funding_vout: 0,
        funding_amount_sats: htlc_amount,
        fee_input: refund_fee_input.clone(),
        destination_script: payment_input.script_pubkey.clone(),
        fee_change_script: refund_fee_input.script_pubkey.clone(),
        fee_change_amount_sats: refund_fee_input.amount_sats - 500,
        fee_sats: 500,
        fee_asset_id: fee_asset,
        kind: SpendKind::Refund,
    })
    .expect("build Liquid refund");
    let signed_refund = sign_liquid_htlc_spend(
        unsigned_refund,
        &contract,
        &refund_fee_input,
        &refund_key,
        &refund_fee_key,
        htlc_amount,
        None,
    )
    .expect("sign Liquid refund");
    assert!(
        broadcast_liquid_transaction(&cross_network_market_maker::chain::BroadcastLiquidArgs {
            rpc,
            raw_hex: &signed_refund.raw_hex,
        },)
        .is_err(),
        "Liquid refund must fail before its CLTV height"
    );
    let current: ChainInfo = rpc
        .call("getblockchaininfo", &[])
        .expect("Elements tip after refund rejection");
    let blocks = contract
        .refund_lock_height
        .saturating_sub(current.blocks)
        .saturating_add(1);
    mine_elements(rpc, &mining_address, blocks);
    let refund_txid =
        broadcast_liquid_transaction(&cross_network_market_maker::chain::BroadcastLiquidArgs {
            rpc,
            raw_hex: &signed_refund.raw_hex,
        })
        .expect("broadcast mature Liquid refund")
        .txid;
    mine_elements(rpc, &mining_address, 1);
    let confirmed: Value = rpc
        .call("getrawtransaction", &[json!(refund_txid), json!(true)])
        .expect("confirmed Liquid refund");
    assert!(
        confirmed
            .get("confirmations")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            >= 1
    );
    println!("liquid refund funding={funding_txid} refund={refund_txid}");
}

fn run_bitcoin_refund(
    rpc: &RpcClient,
    miner: &RpcClient,
    miner_address: &str,
    wallet_key: &SecretKey,
    source: &BitcoinUtxo,
) {
    let tip: ChainInfo = rpc.call("getblockchaininfo", &[]).expect("Bitcoin tip");
    let claim_key = SecretKey::new(&mut OsRng);
    let refund_key = SecretKey::new(&mut OsRng);
    let preimage: [u8; 32] = rand::random();
    let contract = bitcoin_contract(rpc, &claim_key, &refund_key, preimage, tip.blocks + 8);
    let funding = build_bitcoin_funding_transaction(&BitcoinFundingArgs {
        input: source.clone(),
        contract: contract.clone(),
        htlc_amount_sats: 20_000,
        change_script: source.script_pubkey.clone(),
        change_amount_sats: source.amount_sats - 20_000 - 1_000,
        fee_sats: 1_000,
    })
    .expect("build refund funding");
    let signed =
        sign_bitcoin_funding_transaction(funding, source, wallet_key).expect("sign refund funding");
    let funding_txid = broadcast_bitcoin_transaction(&BroadcastBitcoinArgs {
        rpc,
        raw_hex: &signed.raw_hex,
    })
    .expect("broadcast refund funding")
    .txid;
    mine_bitcoin(miner, miner_address, 1);
    let refund = build_bitcoin_htlc_spend(&BitcoinSpendArgs {
        contract: contract.clone(),
        funding_txid: funding_txid.parse().expect("refund funding txid"),
        funding_vout: 0,
        funding_amount_sats: 20_000,
        destination_script: source.script_pubkey.clone(),
        fee_sats: 500,
        kind: SpendKind::Refund,
    })
    .expect("build refund");
    let signed_refund =
        sign_bitcoin_htlc_spend(refund, &contract, &refund_key, 20_000, None).expect("sign refund");
    assert!(
        broadcast_bitcoin_transaction(&BroadcastBitcoinArgs {
            rpc,
            raw_hex: &signed_refund.raw_hex,
        })
        .is_err()
    );
    let current: ChainInfo = rpc
        .call("getblockchaininfo", &[])
        .expect("Bitcoin tip after refund rejection");
    let blocks = contract
        .refund_lock_height
        .saturating_sub(current.blocks)
        .saturating_add(1);
    mine_bitcoin(miner, miner_address, blocks);
    let refund_txid = broadcast_bitcoin_transaction(&BroadcastBitcoinArgs {
        rpc,
        raw_hex: &signed_refund.raw_hex,
    })
    .expect("broadcast mature refund")
    .txid;
    mine_bitcoin(miner, miner_address, 1);
    let confirmed: Value = rpc
        .call("getrawtransaction", &[json!(refund_txid), json!(true)])
        .expect("confirmed refund");
    assert!(
        confirmed
            .get("confirmations")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            >= 1
    );
    println!("bitcoin refund funding={funding_txid} refund={refund_txid}");
}

fn bitcoin_contract(
    rpc: &RpcClient,
    claim_key: &SecretKey,
    refund_key: &SecretKey,
    preimage: [u8; 32],
    additional_lock_blocks: u64,
) -> cross_network_market_maker::chain::HtlcContract {
    let tip: ChainInfo = rpc.call("getblockchaininfo", &[]).expect("Bitcoin tip");
    create_htlc_contract(
        Chain::BitcoinRegtest,
        AssetId::Bitcoin,
        Sha256::digest(preimage).into(),
        public_key(claim_key),
        public_key(refund_key),
        tip.blocks + additional_lock_blocks,
    )
    .expect("Bitcoin contract")
}

fn liquid_contract(
    rpc: &RpcClient,
    claim_key: &SecretKey,
    refund_key: &SecretKey,
    asset_id: AssetId,
    preimage: [u8; 32],
    additional_lock_blocks: u64,
) -> cross_network_market_maker::chain::HtlcContract {
    let tip: ChainInfo = rpc.call("getblockchaininfo", &[]).expect("Elements tip");
    create_htlc_contract(
        Chain::LiquidRegtest,
        asset_id,
        Sha256::digest(preimage).into(),
        public_key(claim_key),
        public_key(refund_key),
        tip.blocks + additional_lock_blocks,
    )
    .expect("Liquid contract")
}

fn bitcoin_rpc(endpoint: &str, env_name: &str, default_password: &str) -> RpcClient {
    RpcClient::new(
        endpoint,
        RPC_USER,
        env::var(env_name).unwrap_or_else(|_| default_password.to_owned()),
        Duration::from_secs(20),
    )
    .expect("Bitcoin RPC client")
}

fn elements_rpc(endpoint: &str, env_name: &str, default_password: &str) -> RpcClient {
    RpcClient::new(
        endpoint,
        RPC_USER,
        env::var(env_name).unwrap_or_else(|_| default_password.to_owned()),
        Duration::from_secs(20),
    )
    .expect("Elements RPC client")
}

fn wallet_rpc(endpoint: &str, wallet: &str, env_name: &str, default_password: &str) -> RpcClient {
    let endpoint = format!("{endpoint}/wallet/{wallet}");
    RpcClient::new(
        endpoint,
        RPC_USER,
        env::var(env_name).unwrap_or_else(|_| default_password.to_owned()),
        Duration::from_secs(20),
    )
    .expect("wallet RPC client")
}

fn create_wallet(rpc: &RpcClient, wallet_name: &str) {
    rpc.call_unit(
        "createwallet",
        &[
            json!(wallet_name),
            json!(false),
            json!(false),
            json!(""),
            json!(false),
            json!(true),
        ],
    )
    .expect("create wallet");
}

fn new_bitcoin_address(rpc: &RpcClient) -> String {
    rpc.call("getnewaddress", &[json!(""), json!("bech32")])
        .expect("Bitcoin address")
}

fn new_liquid_address(rpc: &RpcClient) -> String {
    rpc.call("getnewaddress", &[]).expect("Liquid address")
}

fn mine_bitcoin(rpc: &RpcClient, address: &str, blocks: u64) {
    if blocks == 0 {
        return;
    }

    let _: Vec<String> = rpc
        .call("generatetoaddress", &[json!(blocks), json!(address)])
        .expect("mine Bitcoin blocks");
}

fn mine_elements(rpc: &RpcClient, address: &str, blocks: u64) {
    if blocks == 0 {
        return;
    }

    let _: Vec<String> = rpc
        .call("generatetoaddress", &[json!(blocks), json!(address)])
        .expect("mine Elements blocks");
}

fn bitcoin_key_address(key: &SecretKey) -> String {
    let public_key = bitcoin::PublicKey::new(public_key(key));
    let compressed =
        bitcoin::key::CompressedPublicKey::try_from(public_key).expect("compressed Bitcoin key");

    Address::p2wpkh(&compressed, Network::Regtest).to_string()
}

fn p2wpkh_script(key: &SecretKey) -> ScriptBuf {
    let public_key = bitcoin::PublicKey::new(public_key(key));
    let compressed =
        bitcoin::key::CompressedPublicKey::try_from(public_key).expect("compressed Bitcoin key");

    ScriptBuf::new_p2wpkh(&compressed.wpubkey_hash())
}

fn fund_bitcoin_key(
    rpc: &RpcClient,
    miner: &RpcClient,
    miner_address: &str,
    recipient_address: &str,
    recipient_script: &ScriptBuf,
) -> BitcoinUtxo {
    let txid: String = miner
        .call(
            "sendtoaddress",
            &[json!(recipient_address), json!(0.02_f64)],
        )
        .expect("fund Rust Bitcoin key");
    mine_bitcoin(miner, miner_address, 1);
    let transaction: BitcoinRawTransaction = rpc
        .call("getrawtransaction", &[json!(txid), json!(true)])
        .expect("read Rust Bitcoin funding");
    let output = transaction
        .vout
        .into_iter()
        .find(|output| output.script_pub_key.hex == hex::encode(recipient_script.as_bytes()))
        .expect("funding output script");

    BitcoinUtxo {
        txid,
        vout: output.n,
        amount_sats: parse_rpc_coin_amount(&output.value).expect("funding amount"),
        script_pubkey: recipient_script.clone(),
        confirmations: 1,
    }
}

fn liquid_wallet_utxos(rpc: &RpcClient, asset_id: &AssetId) -> Vec<LiquidUtxo> {
    let entries: Vec<LiquidUnspent> = rpc
        .call("listunspent", &[json!(1), json!(9999999)])
        .expect("Liquid wallet UTXOs");

    entries
        .into_iter()
        .filter(|entry| entry.asset == asset_id.to_hex())
        .filter_map(|entry| liquid_entry_to_utxo(entry).ok())
        .collect()
}

fn prepare_liquid_explicit_outputs(
    rpc: &RpcClient,
    wallet: &RpcClient,
    payment_asset: AssetId,
    fee_asset: AssetId,
    amount_sats: u64,
) {
    let payment_addresses = [
        new_unconfidential_liquid_address(wallet),
        new_unconfidential_liquid_address(wallet),
        new_unconfidential_liquid_address(wallet),
    ];
    let fee_addresses = [
        new_unconfidential_liquid_address(wallet),
        new_unconfidential_liquid_address(wallet),
        new_unconfidential_liquid_address(wallet),
    ];
    let mut amounts = serde_json::Map::new();
    let mut assets = serde_json::Map::new();

    payment_addresses
        .iter()
        .chain(fee_addresses.iter())
        .for_each(|address| {
            amounts.insert(address.clone(), Value::String(decimal_sats(amount_sats)));
        });
    payment_addresses.iter().for_each(|address| {
        assets.insert(address.clone(), Value::String(payment_asset.to_hex()));
    });
    fee_addresses.iter().for_each(|address| {
        assets.insert(address.clone(), Value::String(fee_asset.to_hex()));
    });
    let _: Value = wallet
        .call(
            "sendmany",
            &[
                json!(""),
                Value::Object(amounts),
                json!(1),
                json!("cross-network-market-maker-live-chain"),
                json!([]),
                json!(false),
                json!(1),
                json!("ECONOMICAL"),
                Value::Object(assets),
                json!(true),
            ],
        )
        .expect("prepare explicit Liquid outputs");
    let mining_address = new_liquid_address(wallet);
    mine_elements(rpc, &mining_address, 1);
}

fn new_unconfidential_liquid_address(rpc: &RpcClient) -> String {
    let address = new_liquid_address(rpc);
    let info: Value = rpc
        .call("getaddressinfo", &[json!(address)])
        .expect("Liquid address info");

    info.get("unconfidential")
        .or_else(|| info.get("unconfidential_address"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .expect("unconfidential Liquid address")
}

fn decimal_sats(value: u64) -> String {
    let whole = value / 100_000_000;
    let fraction = value % 100_000_000;

    format!("{whole}.{fraction:08}")
}

fn liquid_wallet_utxo(rpc: &RpcClient, asset_id: &AssetId) -> Option<(LiquidUtxo, SecretKey)> {
    liquid_wallet_utxos(rpc, asset_id)
        .into_iter()
        .max_by_key(|entry| entry.amount_sats)
        .map(|entry| liquid_with_key(rpc, entry))
}

fn liquid_with_key(rpc: &RpcClient, entry: LiquidUtxo) -> (LiquidUtxo, SecretKey) {
    let entries: Vec<LiquidUnspent> = rpc
        .call("listunspent", &[json!(1), json!(9999999)])
        .expect("Liquid wallet UTXOs");
    let address = entries
        .into_iter()
        .find_map(|item| {
            if item.txid == entry.txid && item.vout == entry.vout {
                item.address
            } else {
                None
            }
        })
        .expect("Liquid UTXO address");
    let private_key: String = rpc
        .call("dumpprivkey", &[json!(address)])
        .expect("Liquid private key");
    let key = bitcoin::PrivateKey::from_wif(&private_key)
        .expect("Liquid WIF")
        .inner;

    (entry, key)
}

fn liquid_entry_to_utxo(entry: LiquidUnspent) -> Result<LiquidUtxo, String> {
    if entry.amountcommitment.is_some() || entry.assetcommitment.is_some() {
        return Err("Liquid UTXO is confidential".to_owned());
    }

    let amount = entry
        .amount
        .or(entry.value)
        .ok_or_else(|| "Liquid UTXO lacks explicit amount".to_owned())?;
    let amount_sats = parse_rpc_coin_amount(&amount).map_err(|error| error.to_string())?;
    let script_hex = entry
        .script_pub_key
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            entry
                .script_pub_key
                .get("hex")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| "Liquid UTXO lacks a script".to_owned())?;
    let script = hex::decode(script_hex).map_err(|error| error.to_string())?;

    Ok(LiquidUtxo {
        txid: entry.txid,
        vout: entry.vout,
        asset_id: AssetId::from_hex(&entry.asset).map_err(|error| error.to_string())?,
        amount_sats,
        script_pubkey: ScriptBuf::from_bytes(script),
        confirmations: entry.confirmations,
    })
}

fn public_key(key: &SecretKey) -> PublicKey {
    PublicKey::from_secret_key(&Secp256k1::new(), key)
}

fn require_live_flag() {
    assert_eq!(
        env::var("XMM_RUN_LIVE_CHAIN").as_deref(),
        Ok("1"),
        "set XMM_RUN_LIVE_CHAIN=1 to run real regtest chain tests"
    );
}

fn unique_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
}
