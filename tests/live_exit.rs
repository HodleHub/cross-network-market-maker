#![allow(clippy::too_many_arguments)]

use std::env;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::str::FromStr;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bitcoin::{Address, Network, ScriptBuf, Transaction};
use cross_network_market_maker::chain::parse_rpc_coin_amount;
use cross_network_market_maker::exit::{
    ChannelPoint, CsvSpendProof, ExitChannel, ExitLndClient, ExitRecoveryAccounting,
    HtlcTimeoutProof, IdentifyCsvSpendArgs, IdentifyTimeoutByHashArgs, PendingForceClose,
    account_exit_recovery, identify_csv_spend, identify_htlc_timeout_by_hash,
    parse_exit_transaction,
};
use cross_network_market_maker::lightning::{
    HoldInvoiceRequest, LightningNetwork, LndRestConfig, PaymentRequest, PaymentState,
};
use cross_network_market_maker::rpc::RpcClient;
use cross_network_market_maker::swaps::generate_swap_secrets;
use cross_network_market_maker::swaps::regtest::RegtestSwapHarness;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const BITCOIN_RPC_URL: &str = "http://127.0.0.1:29443";
const RPC_USER: &str = "cross_network_market_maker";
const BITCOIN_RPC_PASSWORD: &str = "cross_network_market_maker_rpc_password";
const EXIT_AMOUNT_SATS: u64 = 23_456;
const HOLD_CLTV_EXPIRY: u32 = 40;
const ROUTER_CLTV_LIMIT: u32 = 80;
const PAYMENT_FEE_LIMIT_SATS: u64 = 50;
const CSV_DELAY: u16 = 144;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(240);

#[derive(Clone, Debug, Deserialize)]
struct ChainInfo {
    chain: String,
    blocks: u64,
}

type ObservedTransaction = (String, Transaction, u64);

#[derive(Clone, Debug, Default)]
struct ExitProofState {
    timeout_txid: Option<String>,
    timeout_height: Option<u64>,
    timeout_output_sats: Option<u64>,
    ordinary_sweep_txid: Option<String>,
    ordinary_recovery_sats: Option<u64>,
    htlc_sweep_txid: Option<String>,
    htlc_recovery_sats: Option<u64>,
    timeout_proof: Option<HtlcTimeoutProof>,
    ordinary_proof: Option<CsvSpendProof>,
    htlc_proof: Option<CsvSpendProof>,
    observed_transactions: Vec<ObservedTransaction>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ExitRecoveryBundle {
    payment_hash_hex: String,
    amount_sats: u64,
    accepted_expiry_height: Option<u64>,
    channel_point: Option<String>,
    closing_txid: Option<String>,
    timeout_txid: Option<String>,
    ordinary_sweep_txid: Option<String>,
    htlc_sweep_txid: Option<String>,
}

struct RecipientGuard {
    stopped: bool,
}

impl RecipientGuard {
    fn stopped() -> Self {
        Self { stopped: true }
    }

    fn restored(&mut self) {
        self.stopped = false;
    }
}

impl Drop for RecipientGuard {
    fn drop(&mut self) {
        if self.stopped {
            let _ = restore_recipient();
        }
    }
}

#[test]
#[ignore = "requires isolated regtest and explicit native-exit opt-in"]
fn native_exit_recovers_exact_accepted_htlc_and_csv_outputs() -> Result<(), Box<dyn Error>> {
    require_live_exit_flag()?;
    let harness = RegtestSwapHarness::load()?;
    let bitcoin = bitcoin_rpc()?;
    qualify_initial_backends(&bitcoin, &harness)?;
    let exit_client = load_exit_client(&harness, "lnd-alice")?;
    exit_client.get_info()?;
    let material = generate_swap_secrets()?;
    let (mut recovery, recovery_path) = new_recovery_bundle(&material)?;
    persist_recovery(&recovery_path, &recovery, true)?;

    let invoice = harness.bob.create_hold_invoice(HoldInvoiceRequest {
        payment_hash: material.hash_commitment,
        amount_sats: EXIT_AMOUNT_SATS,
        cltv_expiry: HOLD_CLTV_EXPIRY,
        memo: Some("cross-network-market-maker native exit".to_owned()),
    })?;
    let payment_request = invoice
        .payment_request
        .clone()
        .ok_or("native-exit invoice has no payment request")?;
    let payment = harness.alice.pay(PaymentRequest {
        payment_request,
        payment_hash: material.hash_commitment,
        expected_amount_sats: EXIT_AMOUNT_SATS,
        fee_limit_sats: PAYMENT_FEE_LIMIT_SATS,
        cltv_limit: ROUTER_CLTV_LIMIT,
        timeout: Duration::from_secs(30),
    });
    let accepted = wait_for_accepted(&harness, material.hash_commitment)?;
    if payment
        .as_ref()
        .map(|value| value.state == PaymentState::Failed)
        .unwrap_or(false)
    {
        return Err("payer reported a failed native-exit payment".into());
    }
    let accepted_expiry_height = accepted
        .accepted_expiry_height
        .ok_or("accepted hold lacks an expiry height")?;
    recovery.accepted_expiry_height = Some(accepted_expiry_height);
    persist_recovery(&recovery_path, &recovery, false)?;

    let channel = wait_for_outgoing_channel(&exit_client, material.hash_commitment)?;
    recovery.channel_point = Some(channel.channel_point.serialized.clone());
    persist_recovery(&recovery_path, &recovery, false)?;

    stop_recipient()?;
    let mut recipient_guard = RecipientGuard::stopped();
    let close = exit_client.force_close(channel.channel_point.clone())?;
    let pending = wait_for_pending_close(&exit_client, &channel.channel_point)?;
    recovery.closing_txid = Some(pending.closing_txid.clone());
    persist_recovery(&recovery_path, &recovery, false)?;

    let miner = create_miner(&bitcoin)?;
    let close_height = confirm_transaction(&bitcoin, &miner, &pending.closing_txid)?;
    let close_transaction = read_transaction(&bitcoin, &pending.closing_txid)?;
    let candidates = htlc_candidates(&close_transaction);
    if candidates.is_empty() {
        return Err("force-close commitment has no target HTLC output candidates".into());
    }
    let owned_scripts = owned_scripts(&exit_client)?;
    let mut state = ExitProofState::default();
    observe_exit_stages(
        &bitcoin,
        &miner,
        &exit_client,
        &pending,
        &close_transaction,
        &candidates,
        material.hash_commitment,
        close_height,
        &owned_scripts,
        &mut state,
        accepted_expiry_height,
    )?;
    let accounting = finalize_exit_proof(&bitcoin, &pending, &state, EXIT_AMOUNT_SATS)?;
    recovery.timeout_txid = state.timeout_txid.clone();
    recovery.ordinary_sweep_txid = state.ordinary_sweep_txid.clone();
    recovery.htlc_sweep_txid = state.htlc_sweep_txid.clone();
    persist_recovery(&recovery_path, &recovery, false)?;
    restore_recipient()?;
    recipient_guard.restored();

    println!(
        "native exit close={} timeout={} ordinary_sweep={} htlc_sweep={} net_recovered_sats={}",
        pending.closing_txid,
        state.timeout_txid.as_deref().unwrap_or("missing"),
        state.ordinary_sweep_txid.as_deref().unwrap_or("missing"),
        state.htlc_sweep_txid.as_deref().unwrap_or("missing"),
        accounting.net_recovered_sats,
    );
    let _ = close;

    Ok(())
}

fn qualify_initial_backends(
    bitcoin: &RpcClient,
    harness: &RegtestSwapHarness,
) -> Result<(), Box<dyn Error>> {
    let info: ChainInfo = bitcoin.call("getblockchaininfo", &[])?;

    if info.chain != "regtest" {
        return Err("native exit requires Bitcoin Core regtest".into());
    }

    let alice = harness.alice.get_info()?;
    let bob = harness.bob.get_info()?;

    if alice.identity_pubkey == bob.identity_pubkey {
        return Err("native exit requires distinct LND identities".into());
    }

    Ok(())
}

fn wait_for_accepted(
    harness: &RegtestSwapHarness,
    payment_hash: [u8; 32],
) -> Result<cross_network_market_maker::lightning::HoldInvoice, Box<dyn Error>> {
    wait_until(OBSERVATION_TIMEOUT, || {
        harness
            .bob
            .lookup_invoice(payment_hash)
            .ok()
            .filter(|invoice| {
                invoice.state == cross_network_market_maker::lightning::InvoiceState::Accepted
            })
    })
    .ok_or_else(|| "timed out waiting for accepted native-exit hold".into())
}

fn wait_for_outgoing_channel(
    client: &ExitLndClient,
    payment_hash: [u8; 32],
) -> Result<ExitChannel, Box<dyn Error>> {
    wait_until(OBSERVATION_TIMEOUT, || {
        client.list_channels().ok().and_then(|channels| {
            channels.into_iter().find(|channel| {
                channel.active
                    && channel.local_balance_sats >= EXIT_AMOUNT_SATS
                    && channel.pending_htlcs.iter().any(|htlc| {
                        htlc.hash_lock == payment_hash
                            && !htlc.incoming
                            && htlc.amount_sats == EXIT_AMOUNT_SATS
                    })
            })
        })
    })
    .ok_or_else(|| "no active outgoing channel carried the accepted target HTLC".into())
}

fn wait_for_pending_close(
    client: &ExitLndClient,
    channel_point: &ChannelPoint,
) -> Result<PendingForceClose, Box<dyn Error>> {
    wait_until(OBSERVATION_TIMEOUT, || {
        client.pending_force_closes().ok().and_then(|pending| {
            pending
                .into_iter()
                .find(|item| item.channel_point == *channel_point)
        })
    })
    .ok_or_else(|| "force-close was not reconciled in LND pending channels".into())
}

fn observe_exit_stages(
    bitcoin: &RpcClient,
    miner: &MinerWallet,
    exit_client: &ExitLndClient,
    pending: &PendingForceClose,
    close_transaction: &Transaction,
    candidates: &[(u32, ScriptBuf)],
    payment_hash: [u8; 32],
    close_height: u64,
    initial_owned_scripts: &[ScriptBuf],
    state: &mut ExitProofState,
    accepted_expiry_height: u64,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + OBSERVATION_TIMEOUT;
    let mut owned = initial_owned_scripts.to_vec();

    while !proofs_complete(state) {
        if Instant::now() >= deadline {
            return Err("native exit proof timed out before both CSV sweeps".into());
        }

        let block = mine_one(bitcoin, miner)?;
        let height = chain_height(bitcoin)?;
        let transactions = block_transactions(bitcoin, &block)?;
        owned = merge_owned_scripts(owned, owned_scripts(exit_client)?);
        transactions.iter().for_each(|transaction| {
            if !state
                .observed_transactions
                .iter()
                .any(|item| item.0 == transaction.0)
            {
                state.observed_transactions.push(transaction.clone());
            }
        });
        let observed_transactions = state.observed_transactions.clone();
        inspect_block_transactions(
            bitcoin,
            &observed_transactions,
            pending,
            close_transaction,
            candidates,
            payment_hash,
            close_height,
            accepted_expiry_height,
            &owned,
            state,
        )?;

        if height < accepted_expiry_height {
            continue;
        }

        sleep(POLL_INTERVAL);
    }

    Ok(())
}

fn inspect_block_transactions(
    bitcoin: &RpcClient,
    transactions: &[ObservedTransaction],
    pending: &PendingForceClose,
    close_transaction: &Transaction,
    candidates: &[(u32, ScriptBuf)],
    payment_hash: [u8; 32],
    close_height: u64,
    accepted_expiry_height: u64,
    owned_scripts: &[ScriptBuf],
    state: &mut ExitProofState,
) -> Result<(), Box<dyn Error>> {
    for (txid, transaction, height) in transactions {
        inspect_timeout_transaction(
            txid,
            transaction,
            pending,
            candidates,
            payment_hash,
            accepted_expiry_height,
            *height,
            state,
        )?;
        let expected_delayed_script = state
            .timeout_proof
            .as_ref()
            .map(|proof| proof.timeout_output_script.clone());
        inspect_ordinary_csv(
            txid,
            transaction,
            &pending.closing_txid,
            close_transaction,
            close_height,
            *height,
            owned_scripts,
            expected_delayed_script.as_ref(),
            state,
        )?;
        inspect_htlc_csv(bitcoin, txid, transaction, *height, owned_scripts, state)?;
    }

    Ok(())
}

fn inspect_timeout_transaction(
    txid: &str,
    transaction: &Transaction,
    pending: &PendingForceClose,
    candidates: &[(u32, ScriptBuf)],
    payment_hash: [u8; 32],
    accepted_expiry_height: u64,
    height: u64,
    state: &mut ExitProofState,
) -> Result<(), Box<dyn Error>> {
    if state.timeout_proof.is_some() {
        return Ok(());
    }

    let raw_hex = hex::encode(bitcoin::consensus::encode::serialize(transaction));
    let proof = identify_htlc_timeout_by_hash(&IdentifyTimeoutByHashArgs {
        raw_hex: &raw_hex,
        commitment_txid: &pending.closing_txid,
        payment_hash,
        candidate_outputs: candidates,
        expected_amount_sats: EXIT_AMOUNT_SATS,
    });

    if let Ok(proof) = proof {
        if proof.timeout_txid != txid || u64::from(proof.lock_time) < accepted_expiry_height {
            return Ok(());
        }

        state.timeout_txid = Some(proof.timeout_txid.clone());
        state.timeout_height = Some(height);
        state.timeout_output_sats = Some(proof.timeout_amount_sats);
        state.timeout_proof = Some(proof);
    }

    Ok(())
}

fn inspect_ordinary_csv(
    txid: &str,
    transaction: &Transaction,
    closing_txid: &str,
    closing_transaction: &Transaction,
    close_height: u64,
    sweep_height: u64,
    owned_scripts: &[ScriptBuf],
    expected_delayed_script: Option<&ScriptBuf>,
    state: &mut ExitProofState,
) -> Result<(), Box<dyn Error>> {
    if state.ordinary_proof.is_some() {
        return Ok(());
    }

    let Some(expected_delayed_script) = expected_delayed_script else {
        return Ok(());
    };

    let raw_hex = hex::encode(bitcoin::consensus::encode::serialize(transaction));

    for input in &transaction.input {
        if input.previous_output.txid.to_string() != closing_txid {
            continue;
        }

        if input.sequence.to_consensus_u32() & 0x0000_ffff < u32::from(CSV_DELAY) {
            continue;
        }

        if closing_transaction
            .output
            .get(input.previous_output.vout as usize)
            .is_none_or(|output| output.script_pubkey != *expected_delayed_script)
        {
            continue;
        }

        let proof = identify_csv_spend(&IdentifyCsvSpendArgs {
            raw_hex: &raw_hex,
            source_txid: closing_txid,
            source_vout: input.previous_output.vout,
            required_csv: CSV_DELAY,
            source_height: close_height,
            sweep_height,
            owned_scripts,
        });

        if let Ok(proof) = proof {
            state.ordinary_sweep_txid = Some(txid.to_owned());
            state.ordinary_recovery_sats = Some(proof.recovered_sats);
            state.ordinary_proof = Some(proof);
            return Ok(());
        }
    }

    Ok(())
}

fn inspect_htlc_csv(
    bitcoin: &RpcClient,
    txid: &str,
    transaction: &Transaction,
    sweep_height: u64,
    owned_scripts: &[ScriptBuf],
    state: &mut ExitProofState,
) -> Result<(), Box<dyn Error>> {
    let Some(timeout_txid) = state.timeout_txid.as_deref() else {
        return Ok(());
    };
    let Some(timeout_height) = state.timeout_height else {
        return Ok(());
    };
    let Some(timeout_proof) = state.timeout_proof.as_ref() else {
        return Ok(());
    };

    if state.htlc_proof.is_some() {
        return Ok(());
    }

    let raw_hex = hex::encode(bitcoin::consensus::encode::serialize(transaction));
    let source_vout = timeout_proof.timeout_output_index as u32;

    for input in &transaction.input {
        if input.previous_output.txid.to_string() != timeout_txid
            || input.previous_output.vout != source_vout
        {
            continue;
        }

        let proof = identify_csv_spend(&IdentifyCsvSpendArgs {
            raw_hex: &raw_hex,
            source_txid: timeout_txid,
            source_vout,
            required_csv: CSV_DELAY,
            source_height: timeout_height,
            sweep_height,
            owned_scripts,
        });

        if let Ok(proof) = proof {
            state.htlc_sweep_txid = Some(txid.to_owned());
            state.htlc_recovery_sats = Some(proof.recovered_sats);
            state.htlc_proof = Some(proof);
            let _ = bitcoin;
            return Ok(());
        }
    }

    Ok(())
}

fn finalize_exit_proof(
    bitcoin: &RpcClient,
    pending: &PendingForceClose,
    state: &ExitProofState,
    principal_sats: u64,
) -> Result<ExitRecoveryAccounting, Box<dyn Error>> {
    let timeout_proof = state
        .timeout_proof
        .as_ref()
        .ok_or("missing timeout proof")?;
    let htlc_proof = state.htlc_proof.as_ref().ok_or("missing HTLC CSV proof")?;
    let timeout_transaction = read_transaction(bitcoin, &timeout_proof.timeout_txid)?;
    let htlc_transaction = read_transaction(bitcoin, &htlc_proof.sweep_txid)?;
    let timeout_fee = transaction_fee_sats(bitcoin, &timeout_transaction)?;
    let htlc_fee = transaction_fee_sats(bitcoin, &htlc_transaction)?;
    let accounting = account_exit_recovery(
        principal_sats,
        timeout_proof.timeout_amount_sats,
        timeout_fee,
        htlc_proof.recovered_sats,
        htlc_fee,
    )?;

    if accounting.net_recovered_sats == 0 {
        return Err("native exit recovered no net target HTLC value".into());
    }

    if state.ordinary_proof.is_none()
        || state.ordinary_recovery_sats.unwrap_or_default() == 0
        || pending.closing_txid.is_empty()
    {
        return Err("ordinary delayed CSV output was not proven".into());
    }

    Ok(accounting)
}

fn transaction_fee_sats(rpc: &RpcClient, transaction: &Transaction) -> Result<u64, Box<dyn Error>> {
    let mut input_sats = 0u64;

    for input in &transaction.input {
        let previous: Value = rpc.call(
            "getrawtransaction",
            &[json!(input.previous_output.txid.to_string()), json!(true)],
        )?;
        let output = previous
            .get("vout")
            .and_then(Value::as_array)
            .and_then(|outputs| {
                outputs.iter().find(|value| {
                    value.get("n").and_then(Value::as_u64)
                        == Some(u64::from(input.previous_output.vout))
                })
            })
            .ok_or("previous output for fee accounting is missing")?;
        let value = output
            .get("value")
            .ok_or("previous output value is missing")?;
        input_sats = input_sats
            .checked_add(parse_rpc_coin_amount(value)?)
            .ok_or("fee input amount overflow")?;
    }

    let output_sats = transaction
        .output
        .iter()
        .map(|output| output.value.to_sat())
        .try_fold(0u64, |total, value| {
            total.checked_add(value).ok_or("fee output overflow")
        })?;

    input_sats
        .checked_sub(output_sats)
        .ok_or_else(|| "transaction outputs exceed inputs".into())
}

fn htlc_candidates(transaction: &Transaction) -> Vec<(u32, ScriptBuf)> {
    transaction
        .output
        .iter()
        .enumerate()
        .map(|(index, output)| (index as u32, output.script_pubkey.clone()))
        .collect()
}

fn proofs_complete(state: &ExitProofState) -> bool {
    state.timeout_proof.is_some() && state.ordinary_proof.is_some() && state.htlc_proof.is_some()
}

fn owned_scripts(client: &ExitLndClient) -> Result<Vec<ScriptBuf>, Box<dyn Error>> {
    let outputs = client.wallet_transaction_outputs()?;

    let scripts = outputs
        .into_iter()
        .filter(|output| output.is_our_address)
        .filter_map(|output| {
            Address::from_str(&output.address)
                .ok()
                .and_then(|address| address.require_network(Network::Regtest).ok())
                .map(|address| address.script_pubkey())
        })
        .collect::<Vec<_>>();

    Ok(scripts)
}

fn merge_owned_scripts(mut left: Vec<ScriptBuf>, right: Vec<ScriptBuf>) -> Vec<ScriptBuf> {
    right.into_iter().for_each(|script| {
        if !left.contains(&script) {
            left.push(script);
        }
    });

    left
}

fn read_transaction(rpc: &RpcClient, txid: &str) -> Result<Transaction, Box<dyn Error>> {
    let value: Value = rpc.call("getrawtransaction", &[json!(txid), json!(true)])?;
    let raw_hex = value
        .get("hex")
        .and_then(Value::as_str)
        .ok_or("raw transaction lacks hex")?;

    Ok(parse_exit_transaction(raw_hex)?)
}

fn block_transactions(
    rpc: &RpcClient,
    block_hash: &str,
) -> Result<Vec<ObservedTransaction>, Box<dyn Error>> {
    let block: Value = rpc.call("getblock", &[json!(block_hash), json!(2)])?;
    let height = block
        .get("height")
        .and_then(Value::as_u64)
        .ok_or("mined block lacks height")?;
    let values = block
        .get("tx")
        .and_then(Value::as_array)
        .ok_or("mined block lacks transactions")?;

    values
        .iter()
        .map(|value| {
            let txid = value
                .get("txid")
                .and_then(Value::as_str)
                .ok_or("block transaction lacks txid")?;
            let raw_hex = value
                .get("hex")
                .and_then(Value::as_str)
                .ok_or("block transaction lacks raw hex")?;
            let transaction = parse_exit_transaction(raw_hex)?;

            Ok((txid.to_owned(), transaction, height))
        })
        .collect()
}

fn confirm_transaction(
    rpc: &RpcClient,
    miner: &MinerWallet,
    txid: &str,
) -> Result<u64, Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        let value: Value = rpc.call("getrawtransaction", &[json!(txid), json!(true)])?;
        let confirmations = value
            .get("confirmations")
            .and_then(Value::as_i64)
            .unwrap_or_default();

        if confirmations >= 1 {
            let block_hash = value
                .get("blockhash")
                .and_then(Value::as_str)
                .ok_or("confirmed transaction lacks block hash")?;
            let block: Value = rpc.call("getblock", &[json!(block_hash), json!(1)])?;
            return block
                .get("height")
                .and_then(Value::as_u64)
                .ok_or_else(|| "confirmed block lacks height".into());
        }

        if Instant::now() >= deadline {
            return Err("force-close transaction was not confirmed".into());
        }

        mine_one(rpc, miner)?;
        sleep(POLL_INTERVAL);
    }
}

#[derive(Clone, Debug)]
struct MinerWallet {
    rpc: RpcClient,
    address: String,
}

fn create_miner(rpc: &RpcClient) -> Result<MinerWallet, Box<dyn Error>> {
    let name = format!("xmm-exit-miner-{}", unique_nonce());
    let miner = RpcClient::new(
        format!("{BITCOIN_RPC_URL}/wallet/{name}"),
        RPC_USER,
        env::var("XMM_BITCOIN_RPC_PASSWORD").unwrap_or_else(|_| BITCOIN_RPC_PASSWORD.to_owned()),
        Duration::from_secs(20),
    )?;
    rpc.call_unit(
        "createwallet",
        &[
            json!(name),
            json!(false),
            json!(false),
            json!(""),
            json!(false),
            json!(true),
        ],
    )?;
    let address: String = miner.call("getnewaddress", &[json!(""), json!("bech32")])?;

    Ok(MinerWallet {
        rpc: miner,
        address,
    })
}

fn mine_one(rpc: &RpcClient, miner: &MinerWallet) -> Result<String, Box<dyn Error>> {
    let blocks: Vec<String> = miner
        .rpc
        .call("generatetoaddress", &[json!(1), json!(&miner.address)])?;
    let block = blocks
        .into_iter()
        .next()
        .ok_or("miner returned no block hash")?;
    let _ = rpc;

    Ok(block)
}

fn chain_height(rpc: &RpcClient) -> Result<u64, Box<dyn Error>> {
    let info: ChainInfo = rpc.call("getblockchaininfo", &[])?;

    Ok(info.blocks)
}

fn bitcoin_rpc() -> Result<RpcClient, Box<dyn Error>> {
    Ok(RpcClient::new(
        BITCOIN_RPC_URL,
        RPC_USER,
        env::var("XMM_BITCOIN_RPC_PASSWORD").unwrap_or_else(|_| BITCOIN_RPC_PASSWORD.to_owned()),
        Duration::from_secs(20),
    )?)
}

fn load_exit_client(
    harness: &RegtestSwapHarness,
    service: &str,
) -> Result<ExitLndClient, Box<dyn Error>> {
    let directory = runtime_dir().join(service);
    let certificate = fs::read(directory.join("tls.cert"))?;
    let macaroon = fs::read_to_string(directory.join("admin.macaroon"))?;
    let endpoint = if service == "lnd-alice" {
        &harness.fixture.lightning.alice.rest_url
    } else {
        &harness.fixture.lightning.bob.rest_url
    };

    Ok(ExitLndClient::new(LndRestConfig {
        base_url: endpoint.to_owned(),
        tls_certificate_pem: certificate,
        macaroon_hex: macaroon.trim().to_owned(),
        request_timeout: Duration::from_secs(30),
        network: LightningNetwork::Regtest,
    })?)
}

fn runtime_dir() -> PathBuf {
    env::var("XMM_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("runtime"))
}

fn new_recovery_bundle(
    material: &cross_network_market_maker::swaps::SwapKeyMaterial,
) -> Result<(ExitRecoveryBundle, PathBuf), Box<dyn Error>> {
    let path = runtime_dir()
        .join("native-exit")
        .join(format!("run-{}", unique_nonce()));
    fs::create_dir_all(&path)?;

    let bundle = ExitRecoveryBundle {
        payment_hash_hex: hex::encode(material.hash_commitment),
        amount_sats: EXIT_AMOUNT_SATS,
        accepted_expiry_height: None,
        channel_point: None,
        closing_txid: None,
        timeout_txid: None,
        ordinary_sweep_txid: None,
        htlc_sweep_txid: None,
    };

    Ok((bundle, path.join("recovery.json")))
}

fn persist_recovery(
    path: &Path,
    bundle: &ExitRecoveryBundle,
    initial: bool,
) -> Result<(), Box<dyn Error>> {
    let bytes = serde_json::to_vec_pretty(bundle)?;

    if initial {
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        file.write_all(&bytes)?;
        set_private_permissions(path)?;

        return Ok(());
    }

    let temporary = path.with_extension(format!("json.{}", unique_nonce()));
    fs::write(&temporary, bytes)?;
    set_private_permissions(&temporary)?;
    fs::rename(temporary, path)?;

    Ok(())
}

fn set_private_permissions(path: &Path) -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

fn stop_recipient() -> Result<(), Box<dyn Error>> {
    run_compose(&["stop", "lnd-bob"])?;

    Ok(())
}

fn restore_recipient() -> Result<(), Box<dyn Error>> {
    run_compose(&["start", "lnd-bob"])?;
    let output = Command::new("bash")
        .arg("scripts/bootstrap-regtest.sh")
        .env("XMM_NETWORK", "regtest")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()?;

    if !output.status.success() {
        return Err("regtest bootstrap did not restore recipient LND".into());
    }

    Ok(())
}

fn run_compose(args: &[&str]) -> Result<Output, Box<dyn Error>> {
    let output = Command::new("docker")
        .args([
            "compose",
            "--project-directory",
            env!("CARGO_MANIFEST_DIR"),
            "--env-file",
            "/dev/null",
            "--file",
            "infra/docker-compose.regtest.yml",
        ])
        .args(args)
        .output()?;

    if !output.status.success() {
        return Err(format!("docker compose action failed with {}", output.status).into());
    }

    Ok(output)
}

fn require_live_exit_flag() -> Result<(), Box<dyn Error>> {
    let enabled = env::var("XMM_RUN_LIVE_EXIT").as_deref() == Ok("1")
        || env::var("ATOMIC_SWAP_POC_EXIT_QUALIFIED").as_deref() == Ok("1");

    if !enabled {
        return Err("set XMM_RUN_LIVE_EXIT=1 for the explicit native-exit qualification".into());
    }

    if env::var("XMM_NETWORK").as_deref().unwrap_or("regtest") != "regtest" {
        return Err("native exit refuses a non-regtest network".into());
    }

    Ok(())
}

fn unique_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn wait_until<T, F>(timeout: Duration, mut read: F) -> Option<T>
where
    F: FnMut() -> Option<T>,
{
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(value) = read() {
            return Some(value);
        }

        if Instant::now() >= deadline {
            return None;
        }

        sleep(POLL_INTERVAL);
    }
}
