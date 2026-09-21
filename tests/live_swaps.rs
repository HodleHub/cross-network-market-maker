use std::error::Error;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use cross_network_market_maker::chain::Chain;
use cross_network_market_maker::lightning::{
    CancelHoldRequest, HoldInvoice, InvoiceState, LightningNetwork, LndRestClient, LndRestConfig,
    PaymentState,
};
use cross_network_market_maker::swaps::regtest::RegtestSwapHarness;
use cross_network_market_maker::swaps::{
    ForwardSwapRequest, NoopSwapObserver, ReverseSwapRequest, SwapChainAdapter, SwapError,
    SwapObserver, SwapPhase, SwapTerms, SwapTermsDefaults, generate_swap_secrets, read_outbox,
    read_recovery, refund_funded_swap, run_forward_swap, run_reverse_swap,
};

const ASSET_AMOUNT_SATS: u64 = 20_000;
const LIGHTNING_AMOUNT_SATS: u64 = 1_000;
const FEE_AMOUNT_SATS: u64 = 500;
const LIQUID_LIQUIDITY_SATS: u64 = 50_000;
const FORWARD_REFUND_DELTA_BLOCKS: u64 = 1_000;
const REVERSE_REFUND_DELTA_BLOCKS: u64 = 30;
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(15);

#[test]
#[ignore = "requires isolated regtest"]
fn test_depix_lightning_forward_reverse_and_replay_use_real_nodes() -> Result<(), Box<dyn Error>> {
    require_live_flag("XMM_SWAP_INTEGRATION")?;
    let harness = RegtestSwapHarness::load()?;
    let asset_id = cross_network_market_maker::chain::AssetId::from_hex(
        &harness.fixture.elements.test_depix_asset_id,
    )?;
    let fee_asset_id = cross_network_market_maker::chain::AssetId::from_hex(
        &harness.fixture.elements.policy_asset_id,
    )?;

    let forward = run_forward_with_replay(&harness, asset_id, fee_asset_id)?;
    assert!(forward.preimage_verified);
    assert!(forward.invoice_settled);
    assert!(forward.payer_succeeded);

    let reverse = run_reverse_with_replay(&harness, asset_id, fee_asset_id)?;
    assert!(reverse.preimage_verified);
    assert!(reverse.invoice_settled);
    assert!(reverse.payer_succeeded);

    Ok(())
}

#[test]
#[ignore = "requires isolated regtest"]
fn lnd_rejects_a_wrong_pinned_node_certificate() -> Result<(), Box<dyn Error>> {
    require_live_flag("XMM_LIGHTNING_INTEGRATION")?;
    let harness = RegtestSwapHarness::load()?;
    let runtime_dir = std::env::var("XMM_RUNTIME_DIR").unwrap_or_else(|_| "runtime".to_owned());
    let alice_certificate = fs::read(format!("{runtime_dir}/lnd-alice/tls.cert"))?;
    let bob_certificate = fs::read(format!("{runtime_dir}/lnd-bob/tls.cert"))?;
    if alice_certificate == bob_certificate {
        return Err("dedicated LND nodes unexpectedly share a TLS certificate".into());
    }

    let macaroon = fs::read_to_string(format!("{runtime_dir}/lnd-alice/admin.macaroon"))?;
    let wrong_client = LndRestClient::new(LndRestConfig {
        base_url: harness.fixture.lightning.alice.rest_url.clone(),
        tls_certificate_pem: bob_certificate,
        macaroon_hex: macaroon.trim().to_owned(),
        request_timeout: Duration::from_secs(5),
        network: LightningNetwork::Regtest,
    })?;
    if wrong_client.get_info().is_ok() {
        return Err("LND accepted a certificate from the other node".into());
    }

    Ok(())
}

#[test]
#[ignore = "requires isolated regtest"]
fn test_depix_failed_payment_refunds_after_liquid_timeout() -> Result<(), Box<dyn Error>> {
    require_live_flag("XMM_SWAP_FAILURE_INTEGRATION")?;
    let harness = RegtestSwapHarness::load()?;
    let asset_id = cross_network_market_maker::chain::AssetId::from_hex(
        &harness.fixture.elements.test_depix_asset_id,
    )?;
    let fee_asset_id = cross_network_market_maker::chain::AssetId::from_hex(
        &harness.fixture.elements.policy_asset_id,
    )?;
    harness.prepare_liquidity(LIQUID_LIQUIDITY_SATS)?;
    let name = unique_name("failure");
    let terms = terms(
        &harness,
        asset_id,
        fee_asset_id,
        FORWARD_REFUND_DELTA_BLOCKS,
        &name,
    )?;
    let paths = harness.paths(&name);
    let encryption_key = harness.encryption_key(&name)?;
    let material = generate_swap_secrets()?;
    let invoice = harness.alice.create_hold_invoice(
        cross_network_market_maker::lightning::HoldInvoiceRequest {
            payment_hash: material.hash_commitment,
            amount_sats: LIGHTNING_AMOUNT_SATS,
            cltv_expiry: terms.hold_cltv_expiry,
            memo: Some("cross-network-market-maker live failure".to_owned()),
        },
    )?;
    let payment_request = invoice
        .payment_request
        .clone()
        .ok_or("failure invoice did not return a payment request")?;
    let payment = harness
        .bob
        .pay(cross_network_market_maker::lightning::PaymentRequest {
            payment_request,
            payment_hash: material.hash_commitment,
            expected_amount_sats: LIGHTNING_AMOUNT_SATS,
            fee_limit_sats: terms.fee_limit_sats,
            cltv_limit: terms.lightning_cltv_limit,
            timeout: Duration::from_secs(15),
        })?;
    if payment.state == PaymentState::Failed {
        return Err("failure payment failed before its accepted hold could be canceled".into());
    }

    wait_for_accepted_invoice(
        &harness.alice,
        material.hash_commitment,
        LIGHTNING_AMOUNT_SATS,
    )?;
    let canceler = CancelAfterFunding {
        receiver: &harness.alice,
        payment_hash: material.hash_commitment,
        canceled: AtomicBool::new(false),
    };
    let failed = run_forward_swap(ForwardSwapRequest {
        chain: &harness.elements,
        receiver: &harness.alice,
        payer: &harness.bob,
        paths: paths.clone(),
        encryption_key: encryption_key.clone(),
        terms: terms.clone(),
        supplied_material: Some(material),
        memo: Some("cross-network-market-maker live failure replay".to_owned()),
        observer: &canceler,
    });
    if failed.is_ok() {
        return Err("canceled Lightning payment unexpectedly settled".into());
    }

    if !canceler.canceled.load(Ordering::SeqCst) {
        return Err("failure observer did not cancel after Liquid funding".into());
    }

    let recovery = read_recovery(&paths.recovery, &encryption_key.0)?
        .ok_or("failure recovery record is missing")?;
    let payment =
        wait_for_terminal_payment(&harness.bob, recovery.session.material.hash_commitment)?;
    if payment.state != PaymentState::Failed || payment.payment_preimage.is_some() {
        return Err("failed payment did not remain terminal and preimage-free".into());
    }

    let canceled = harness
        .alice
        .lookup_invoice(recovery.session.material.hash_commitment)?;
    if canceled.state != InvoiceState::Canceled || canceled.amount_sats != LIGHTNING_AMOUNT_SATS {
        return Err("receiver invoice was not canceled with the quoted amount".into());
    }

    let early = refund_funded_swap(
        &harness.elements,
        &harness.alice,
        &harness.bob,
        &paths,
        &encryption_key,
        LIGHTNING_AMOUNT_SATS,
    );
    if early.is_ok() || read_outbox(&paths.refund)?.is_some() {
        return Err("refund was accepted before the Liquid timeout".into());
    }

    let current_height = harness.elements.current_height()?;
    let blocks_to_maturity = recovery
        .contract
        .refund_lock_height
        .saturating_sub(current_height)
        .saturating_add(1);
    harness.elements.mine_blocks(blocks_to_maturity)?;
    let refund = refund_funded_swap(
        &harness.elements,
        &harness.alice,
        &harness.bob,
        &paths,
        &encryption_key,
        LIGHTNING_AMOUNT_SATS,
    )?;
    harness.elements.verify_refund_output(
        &refund.refund_txid,
        asset_id,
        ASSET_AMOUNT_SATS,
        &recovery.session.material.destination_public_key,
    )?;
    let replay = refund_funded_swap(
        &harness.elements,
        &harness.alice,
        &harness.bob,
        &paths,
        &encryption_key,
        LIGHTNING_AMOUNT_SATS,
    )?;
    if replay.refund_txid != refund.refund_txid {
        return Err("refund replay created a different transaction".into());
    }

    println!(
        "live_failure_refund funding_txid={} refund_txid={} asset_id={} amount_sats={} confirmations={}",
        refund.funding_txid,
        refund.refund_txid,
        asset_id.to_hex(),
        ASSET_AMOUNT_SATS,
        refund.confirmations,
    );

    Ok(())
}

fn run_forward_with_replay(
    harness: &RegtestSwapHarness,
    asset_id: cross_network_market_maker::chain::AssetId,
    fee_asset_id: cross_network_market_maker::chain::AssetId,
) -> Result<cross_network_market_maker::swaps::CombinedSwapEvidence, Box<dyn Error>> {
    harness.prepare_liquidity(LIQUID_LIQUIDITY_SATS)?;
    let name = unique_name("forward");
    let terms = terms(
        harness,
        asset_id,
        fee_asset_id,
        FORWARD_REFUND_DELTA_BLOCKS,
        &name,
    )?;
    let paths = harness.paths(&name);
    let encryption_key = harness.encryption_key(&name)?;
    let stopper = StopAfterOutputSettled::default();
    let first = run_forward_swap(ForwardSwapRequest {
        chain: &harness.elements,
        receiver: &harness.alice,
        payer: &harness.bob,
        paths: paths.clone(),
        encryption_key: encryption_key.clone(),
        terms: terms.clone(),
        supplied_material: None,
        memo: Some("cross-network-market-maker forward".to_owned()),
        observer: &stopper,
    });

    if first.is_ok() {
        return Err("fault-injected forward run unexpectedly completed before replay".into());
    }

    if !stopper.triggered.load(Ordering::SeqCst) {
        return Err("forward run stopped before the verified settlement phase".into());
    }

    if read_outbox(&paths.claim)?.is_some() || read_outbox(&paths.funding)?.is_none() {
        return Err("forward stop did not leave the durable funding boundary".into());
    }

    let recovery = read_recovery(&paths.recovery, &encryption_key.0)?
        .ok_or("forward recovery record is missing")?;
    let payment = harness.bob.track_by_hash(
        recovery.session.material.hash_commitment,
        Duration::from_secs(15),
    )?;
    if payment.state != PaymentState::Succeeded
        || payment.payment_preimage != Some(recovery.session.material.preimage)
    {
        return Err("forward stop did not leave a verified payer success to replay".into());
    }

    let replay = run_forward_swap(ForwardSwapRequest {
        chain: &harness.elements,
        receiver: &harness.alice,
        payer: &harness.bob,
        paths,
        encryption_key,
        terms,
        supplied_material: None,
        memo: Some("cross-network-market-maker forward replay".to_owned()),
        observer: &NoopSwapObserver,
    })?;

    print_public_evidence("forward", &recovery, &replay);

    Ok(replay)
}

fn run_reverse_with_replay(
    harness: &RegtestSwapHarness,
    asset_id: cross_network_market_maker::chain::AssetId,
    fee_asset_id: cross_network_market_maker::chain::AssetId,
) -> Result<cross_network_market_maker::swaps::CombinedSwapEvidence, Box<dyn Error>> {
    harness.prepare_liquidity(LIQUID_LIQUIDITY_SATS)?;
    let name = unique_name("reverse");
    let terms = terms(
        harness,
        asset_id,
        fee_asset_id,
        REVERSE_REFUND_DELTA_BLOCKS,
        &name,
    )?;
    let paths = harness.paths(&name);
    let encryption_key = harness.encryption_key(&name)?;
    let request = ReverseSwapRequest {
        chain: &harness.elements,
        receiver: &harness.bob,
        payer: &harness.alice,
        paths: paths.clone(),
        encryption_key: encryption_key.clone(),
        terms: terms.clone(),
        supplied_material: None,
        memo: Some("cross-network-market-maker reverse".to_owned()),
        observer: &NoopSwapObserver,
    };
    let evidence = run_reverse_swap(request)?;
    let recovery = read_recovery(&paths.recovery, &encryption_key.0)?
        .ok_or("reverse recovery record is missing")?;
    let replay = run_reverse_swap(ReverseSwapRequest {
        chain: &harness.elements,
        receiver: &harness.bob,
        payer: &harness.alice,
        paths: paths.clone(),
        encryption_key: encryption_key.clone(),
        terms: terms.clone(),
        supplied_material: None,
        memo: Some("cross-network-market-maker reverse replay".to_owned()),
        observer: &NoopSwapObserver,
    })?;

    if evidence.funding_txid != replay.funding_txid || evidence.claim_txid != replay.claim_txid {
        return Err("reverse replay did not reuse durable transaction outboxes".into());
    }

    print_public_evidence("reverse", &recovery, &replay);

    Ok(replay)
}

fn print_public_evidence(
    direction: &str,
    recovery: &cross_network_market_maker::swaps::SwapRecoveryRecord,
    evidence: &cross_network_market_maker::swaps::CombinedSwapEvidence,
) {
    println!(
        "live_swap direction={direction} funding_txid={} claim_txid={} hash_lock={} amount_sats={} accepted_expiry_height={:?} claim_confirmations={} preimage_verified={}",
        evidence.funding_txid,
        evidence.claim_txid,
        hex::encode(recovery.session.material.hash_commitment),
        recovery.session.asset_amount_sats,
        evidence.accepted_expiry_height,
        evidence.claim_confirmations,
        evidence.preimage_verified,
    );
}

fn terms(
    harness: &RegtestSwapHarness,
    asset_id: cross_network_market_maker::chain::AssetId,
    fee_asset_id: cross_network_market_maker::chain::AssetId,
    refund_delta_blocks: u64,
    quote_id: &str,
) -> Result<SwapTerms, Box<dyn Error>> {
    let height = harness.elements.current_height()?;
    let refund_lock_height = height
        .checked_add(refund_delta_blocks)
        .ok_or("refund height overflow")?;

    Ok(SwapTerms::with_defaults(SwapTermsDefaults {
        chain: Chain::LiquidRegtest,
        asset_id,
        fee_asset_id,
        amount_sats: LIGHTNING_AMOUNT_SATS,
        asset_amount_sats: ASSET_AMOUNT_SATS,
        fee_sats: FEE_AMOUNT_SATS,
        refund_lock_height,
        quote_id: Some(quote_id.to_owned()),
    }))
}

#[derive(Default)]
struct StopAfterOutputSettled {
    triggered: AtomicBool,
}

struct CancelAfterFunding<'a> {
    receiver: &'a LndRestClient,
    payment_hash: [u8; 32],
    canceled: AtomicBool,
}

impl SwapObserver for CancelAfterFunding<'_> {
    fn on_phase(&self, phase: SwapPhase) -> Result<(), SwapError> {
        if phase != SwapPhase::OutputPending || self.canceled.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        self.receiver
            .cancel_hold(CancelHoldRequest {
                payment_hash: self.payment_hash,
                deadline: Some(std::time::SystemTime::now() + Duration::from_secs(60)),
            })
            .map(|_| ())
            .map_err(|error| SwapError::Lightning(error.to_string()))
    }
}

impl SwapObserver for StopAfterOutputSettled {
    fn on_phase(&self, phase: SwapPhase) -> Result<(), SwapError> {
        if phase == SwapPhase::OutputSettled && !self.triggered.swap(true, Ordering::SeqCst) {
            return Err(SwapError::Unknown(
                "test stopped after verified Lightning settlement".to_owned(),
            ));
        }

        Ok(())
    }
}

fn unique_name(direction: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();

    format!("{direction}-{nonce}")
}

fn require_live_flag(name: &str) -> Result<(), Box<dyn Error>> {
    if std::env::var(name).as_deref() != Ok("1") {
        return Err(format!("{name}=1 is required for this live test").into());
    }

    Ok(())
}

fn wait_for_accepted_invoice(
    client: &LndRestClient,
    payment_hash: [u8; 32],
    amount_sats: u64,
) -> Result<HoldInvoice, Box<dyn Error>> {
    wait_until(OBSERVATION_TIMEOUT, || {
        client.lookup_invoice(payment_hash).ok().filter(|invoice| {
            invoice.state == InvoiceState::Accepted
                && invoice.accepted_amount_sats == Some(amount_sats)
        })
    })
    .ok_or_else(|| "timed out waiting for the failure invoice to be accepted".into())
}

fn wait_for_terminal_payment(
    client: &LndRestClient,
    payment_hash: [u8; 32],
) -> Result<cross_network_market_maker::lightning::PaymentObservation, Box<dyn Error>> {
    wait_until(OBSERVATION_TIMEOUT, || {
        client
            .track_by_hash(payment_hash, Duration::from_secs(5))
            .ok()
            .filter(|payment| {
                payment.state == PaymentState::Succeeded || payment.state == PaymentState::Failed
            })
    })
    .ok_or_else(|| "timed out waiting for failed payment reconciliation".into())
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

        sleep(Duration::from_millis(200));
    }
}
