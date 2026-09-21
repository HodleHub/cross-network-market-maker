use std::error::Error;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use cross_network_market_maker::core::storage::{ReservationState, SqliteStore};
use cross_network_market_maker::swaps::regtest::RegtestSwapHarness;
use serde::Deserialize;

const SOLVER_A_BIND: &str = "127.0.0.1:39271";
const SOLVER_B_BIND: &str = "127.0.0.1:39272";
const ASSET_AMOUNT_SATS: &str = "20000";
const LIGHTNING_AMOUNT_SATS: &str = "1000";
const FEE_LIMIT_LBTC: &str = "1000";
const FEE_SATS: &str = "500";
const INVENTORY_AMOUNT_SATS: &str = "1000000";

#[derive(Debug, Deserialize)]
struct LiveOutput {
    state: String,
    funding_txid: String,
    claim_txid: String,
    solver_reservation_consumed: bool,
    replay: bool,
}

#[test]
#[ignore = "requires isolated regtest and two loopback solver peers"]
fn cli_forward_persists_payment_boundary_and_replays_without_repayment()
-> Result<(), Box<dyn Error>> {
    require_live_flag("XMM_RUN_LIVE_FORWARD")?;
    let harness = RegtestSwapHarness::load()?;
    let directory = std::env::var("XMM_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("runtime"))
        .join(format!("cli-forward-{}", unique_suffix()));
    std::fs::create_dir_all(&directory)?;
    let asset_hash = harness.fixture.elements.test_depix_asset_id.clone();
    let maker_a = spawn_solver(
        &directory,
        "maker-a",
        "key-a",
        SOLVER_A_BIND,
        &asset_hash,
        "10",
    )?;
    let maker_b = spawn_solver(
        &directory,
        "maker-b",
        "key-b",
        SOLVER_B_BIND,
        &asset_hash,
        "5",
    )?;
    harness.prepare_liquidity(50_000)?;
    let session = format!("cli-forward-{}", unique_suffix());
    let session_dir = directory.join(&session);
    std::fs::create_dir_all(&session_dir)?;
    let intent_path = session_dir.join("intent.json");
    let selected_path = session_dir.join("selected.json");
    let client_database = session_dir.join("client.sqlite");
    let recovery_key = session_dir.join("recovery.key");
    let prepared = run_success(&prepare_args(
        &session,
        &intent_path,
        &client_database,
        &recovery_key,
        &asset_hash,
    ))?;
    if !String::from_utf8_lossy(&prepared.stdout).contains("payment_request") {
        return Err("prepare did not print the public invoice binding".into());
    }
    let intent: serde_json::Value = serde_json::from_slice(&std::fs::read(&intent_path)?)?;
    let hash_commitment = intent
        .get("hash_commitment")
        .and_then(serde_json::Value::as_str)
        .ok_or("prepared intent has no public hash commitment")?;
    let rfq = run_success(&rfq_args(
        &intent_path,
        &selected_path,
        &asset_hash,
        &maker_a,
        &maker_b,
    ))?;
    if !String::from_utf8_lossy(&rfq.stdout).contains("maker-b") {
        return Err("RFQ did not select the lower-fee maker".into());
    }
    let selected: serde_json::Value = serde_json::from_slice(&std::fs::read(&selected_path)?)?;
    let reservation_id = selected
        .get("reservation_id")
        .and_then(serde_json::Value::as_str)
        .ok_or("selected quote has no reservation id")?
        .to_owned();
    let invocation = SwapInvocation {
        intent: &intent_path,
        selected: &selected_path,
        database: &client_database,
        recovery_key: &recovery_key,
        asset_hash: &asset_hash,
    };
    let forward_args = swap_args("forward", invocation, &maker_b, &maker_a);
    let interrupted = run_cli(&append_flag(&forward_args, "--stop-after-payment"));
    if interrupted.status.success() {
        return Err("stop-after-payment unexpectedly completed the claim".into());
    }
    let status = run_success(&status_args(&client_database, &session))?;
    if !String::from_utf8_lossy(&status.stdout).contains("\"state\": \"OUTPUT_SETTLED\"") {
        return Err("payment boundary did not persist OUTPUT_SETTLED".into());
    }
    let resumed = parse_live(run_success(&swap_args(
        "resume", invocation, &maker_b, &maker_a,
    ))?)?;
    let replayed = parse_live(run_success(&swap_args(
        "resume", invocation, &maker_b, &maker_a,
    ))?)?;
    assert_eq!(resumed.state, "SETTLED");
    assert_eq!(replayed.state, "SETTLED");
    assert!(resumed.replay);
    assert!(replayed.replay);
    assert!(resumed.solver_reservation_consumed);
    assert!(replayed.solver_reservation_consumed);
    assert_eq!(resumed.funding_txid, replayed.funding_txid);
    assert_eq!(resumed.claim_txid, replayed.claim_txid);
    println!(
        "{{\"session\":\"{}\",\"maker\":\"maker-b\",\"funding_txid\":\"{}\",\"claim_txid\":\"{}\",\"replay_same_txids\":true,\"reservation_state\":\"CONSUMED\"}}",
        session, resumed.funding_txid, resumed.claim_txid
    );
    println!("{{\"hash_commitment\":\"{}\"}}", hash_commitment);
    let solver_store = SqliteStore::open(&maker_b.database)?;
    let reservation = solver_store
        .get_reservation(&reservation_id)?
        .ok_or("selected reservation disappeared")?;
    assert_eq!(reservation.state, ReservationState::Consumed);
    drop(maker_a);
    drop(maker_b);

    Ok(())
}

struct MakerProcess {
    database: PathBuf,
    public_key: String,
    child: Child,
}

impl Drop for MakerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_solver(
    directory: &Path,
    solver_id: &str,
    key_id: &str,
    bind: &str,
    asset_hash: &str,
    fee_lbtc: &str,
) -> Result<MakerProcess, Box<dyn Error>> {
    let maker_dir = directory.join(solver_id);
    std::fs::create_dir_all(&maker_dir)?;
    let key_path = maker_dir.join("signing.key");
    let database = maker_dir.join("ledger.sqlite");
    let public_key = keygen(&key_path)?;
    let key_path_text = path(&key_path);
    let database_text = path(&database);
    let child = Command::new(env!("CARGO_BIN_EXE_xmm"))
        .env("XMM_NETWORK", "regtest")
        .args([
            "solver",
            "serve",
            "--bind",
            bind,
            "--solver-id",
            solver_id,
            "--key-id",
            key_id,
            "--key-file",
            &key_path_text,
            "--database",
            &database_text,
            "--asset-hash",
            asset_hash,
            "--inventory-asset-id",
            "BTC",
            "--inventory-amount",
            INVENTORY_AMOUNT_SATS,
            "--rate-numerator",
            "1",
            "--rate-denominator",
            "20",
            "--fee-lbtc",
            fee_lbtc,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    wait_for_peer(bind)?;

    Ok(MakerProcess {
        database,
        public_key,
        child,
    })
}

fn keygen(path: &Path) -> Result<String, Box<dyn Error>> {
    let output = run_success(&[
        "solver".to_owned(),
        "keygen".to_owned(),
        "--out".to_owned(),
        path.display().to_string(),
    ])?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    value
        .get("public_key")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "keygen did not return a public key".into())
}

fn prepare_args(
    session: &str,
    intent: &Path,
    database: &Path,
    recovery_key: &Path,
    asset_hash: &str,
) -> Vec<String> {
    vec![
        "swap".to_owned(),
        "prepare".to_owned(),
        "--session".to_owned(),
        session.to_owned(),
        "--intent-out".to_owned(),
        path(intent),
        "--database".to_owned(),
        path(database),
        "--recovery-key".to_owned(),
        path(recovery_key),
        "--asset-hash".to_owned(),
        asset_hash.to_owned(),
        "--amount-in".to_owned(),
        ASSET_AMOUNT_SATS.to_owned(),
        "--amount-out".to_owned(),
        LIGHTNING_AMOUNT_SATS.to_owned(),
        "--fee-limit-lbtc".to_owned(),
        FEE_LIMIT_LBTC.to_owned(),
        "--fee-sats".to_owned(),
        FEE_SATS.to_owned(),
        "--receiver-url".to_owned(),
        "https://127.0.0.1:29081".to_owned(),
        "--receiver-cert".to_owned(),
        runtime_file("lnd-alice", "tls.cert"),
        "--receiver-macaroon".to_owned(),
        runtime_file("lnd-alice", "admin.macaroon"),
    ]
}

fn rfq_args(
    intent: &Path,
    selected: &Path,
    asset_hash: &str,
    maker_a: &MakerProcess,
    maker_b: &MakerProcess,
) -> Vec<String> {
    vec![
        "rfq".to_owned(),
        "--peer".to_owned(),
        format!("{SOLVER_A_BIND},{SOLVER_B_BIND}"),
        "--intent".to_owned(),
        path(intent),
        "--pinned-key".to_owned(),
        format!("key-a={},key-b={}", maker_a.public_key, maker_b.public_key),
        "--asset-hash".to_owned(),
        asset_hash.to_owned(),
        "--reserve".to_owned(),
        "--out".to_owned(),
        path(selected),
    ]
}

#[derive(Clone, Copy)]
struct SwapInvocation<'a> {
    intent: &'a Path,
    selected: &'a Path,
    database: &'a Path,
    recovery_key: &'a Path,
    asset_hash: &'a str,
}

fn swap_args(
    command: &str,
    invocation: SwapInvocation<'_>,
    maker: &MakerProcess,
    other_maker: &MakerProcess,
) -> Vec<String> {
    vec![
        "swap".to_owned(),
        command.to_owned(),
        "--intent".to_owned(),
        path(invocation.intent),
        "--quote".to_owned(),
        path(invocation.selected),
        "--database".to_owned(),
        path(invocation.database),
        "--recovery-key".to_owned(),
        path(invocation.recovery_key),
        "--pinned-key".to_owned(),
        format!(
            "key-a={},key-b={}",
            other_maker.public_key, maker.public_key
        ),
        "--asset-hash".to_owned(),
        invocation.asset_hash.to_owned(),
        "--solver-database".to_owned(),
        path(&maker.database),
        "--receiver-url".to_owned(),
        "https://127.0.0.1:29081".to_owned(),
        "--receiver-cert".to_owned(),
        runtime_file("lnd-alice", "tls.cert"),
        "--receiver-macaroon".to_owned(),
        runtime_file("lnd-alice", "admin.macaroon"),
        "--payer-url".to_owned(),
        "https://127.0.0.1:29082".to_owned(),
        "--payer-cert".to_owned(),
        runtime_file("lnd-bob", "tls.cert"),
        "--payer-macaroon".to_owned(),
        runtime_file("lnd-bob", "admin.macaroon"),
        "--fee-sats".to_owned(),
        FEE_SATS.to_owned(),
    ]
}

fn status_args(database: &Path, swap_id: &str) -> Vec<String> {
    vec![
        "swap".to_owned(),
        "status".to_owned(),
        "--database".to_owned(),
        path(database),
        "--swap-id".to_owned(),
        swap_id.to_owned(),
    ]
}

fn append_flag(args: &[String], flag: &str) -> Vec<String> {
    let mut result = args.to_vec();
    result.push(flag.to_owned());

    result
}

fn parse_live(output: Output) -> Result<LiveOutput, Box<dyn Error>> {
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn run_success(args: &[String]) -> Result<Output, Box<dyn Error>> {
    let output = run_cli(args);

    if !output.status.success() {
        return Err(format!(
            "xmm failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    Ok(output)
}

fn run_cli(args: &[String]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xmm"))
        .env("XMM_NETWORK", "regtest")
        .args(args)
        .output()
        .expect("xmm binary")
}

fn wait_for_peer(address: &str) -> Result<(), Box<dyn Error>> {
    for _ in 0..80 {
        if TcpStream::connect(address).is_ok() {
            return Ok(());
        }

        thread::sleep(Duration::from_millis(25));
    }

    Err(format!("solver peer did not bind: {address}").into())
}

fn path(path: &Path) -> String {
    path.display().to_string()
}

fn runtime_file(node: &str, file: &str) -> String {
    let runtime = std::env::var("XMM_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("runtime"));

    path(&runtime.join(node).join(file))
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn require_live_flag(name: &str) -> Result<(), Box<dyn Error>> {
    let enabled = std::env::var(name).as_deref() == Ok("1")
        || std::env::var("XMM_RUN_LIVE_SWAP").as_deref() == Ok("1");

    if !enabled {
        return Err(format!("{name}=1 is required for this live test").into());
    }

    Ok(())
}
