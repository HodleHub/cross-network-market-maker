#![allow(clippy::too_many_arguments)]

use std::error::Error;
use std::thread::sleep;
use std::time::{Duration, Instant};

use cross_network_market_maker::lightning::{
    CancelHoldRequest, HoldInvoice, HoldInvoiceRequest, InvoiceState, LightningAdapter,
    PaymentRequest, PaymentState,
};
use cross_network_market_maker::swaps::generate_swap_secrets;
use cross_network_market_maker::swaps::regtest::RegtestSwapHarness;

const PAYMENT_AMOUNT_SATS: u64 = 1_000;
const HOLD_CLTV_EXPIRY: u32 = 40;
const ROUTER_CLTV_LIMIT: u32 = 80;
const FEE_LIMIT_SATS: u64 = 20;
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(15);

#[test]
#[ignore = "requires isolated regtest"]
fn lnd_regtest_hold_settle_and_cancel_use_real_nodes() -> Result<(), Box<dyn Error>> {
    require_live_flag("XMM_LIGHTNING_INTEGRATION")?;
    let harness = RegtestSwapHarness::load()?;

    harness.alice.get_info()?;
    harness.bob.get_info()?;
    run_settle_case(&harness)?;
    run_cancel_case(&harness)?;

    Ok(())
}

fn run_settle_case(harness: &RegtestSwapHarness) -> Result<(), Box<dyn Error>> {
    let material = generate_swap_secrets()?;
    let invoice = harness.alice.create_hold_invoice(HoldInvoiceRequest {
        payment_hash: material.hash_commitment,
        amount_sats: PAYMENT_AMOUNT_SATS,
        cltv_expiry: HOLD_CLTV_EXPIRY,
        memo: Some("cross-network-market-maker live settle".to_owned()),
    })?;
    let payment_request = required_payment_request(&invoice)?;
    let payment = harness.bob.pay(PaymentRequest {
        payment_request,
        payment_hash: material.hash_commitment,
        expected_amount_sats: PAYMENT_AMOUNT_SATS,
        fee_limit_sats: FEE_LIMIT_SATS,
        cltv_limit: ROUTER_CLTV_LIMIT,
        timeout: OBSERVATION_TIMEOUT,
    })?;

    if payment.state == PaymentState::Failed {
        return Err("real hold payment failed before acceptance".into());
    }

    let accepted = wait_for_accepted(
        &harness.alice,
        material.hash_commitment,
        PAYMENT_AMOUNT_SATS,
    )?;
    if accepted.state != InvoiceState::Accepted {
        return Err("real hold invoice did not reach ACCEPTED".into());
    }

    harness
        .alice
        .settle_hold(material.hash_commitment, material.preimage)?;
    let settled = harness.alice.lookup_invoice(material.hash_commitment)?;
    if settled.state != InvoiceState::Settled {
        return Err("real hold invoice did not reach SETTLED".into());
    }

    let tracked = wait_for_payment(&harness.bob, material.hash_commitment)?;
    if tracked.state != PaymentState::Succeeded
        || tracked.payment_preimage != Some(material.preimage)
    {
        return Err("payer did not observe a hash-verified Lightning success".into());
    }

    Ok(())
}

fn run_cancel_case(harness: &RegtestSwapHarness) -> Result<(), Box<dyn Error>> {
    let material = generate_swap_secrets()?;
    let invoice = harness.alice.create_hold_invoice(HoldInvoiceRequest {
        payment_hash: material.hash_commitment,
        amount_sats: PAYMENT_AMOUNT_SATS,
        cltv_expiry: HOLD_CLTV_EXPIRY,
        memo: Some("cross-network-market-maker live cancel".to_owned()),
    })?;
    let payment_request = required_payment_request(&invoice)?;
    let payment = harness.bob.pay(PaymentRequest {
        payment_request,
        payment_hash: material.hash_commitment,
        expected_amount_sats: PAYMENT_AMOUNT_SATS,
        fee_limit_sats: FEE_LIMIT_SATS,
        cltv_limit: ROUTER_CLTV_LIMIT,
        timeout: OBSERVATION_TIMEOUT,
    })?;

    if payment.state == PaymentState::Failed {
        return Err("real cancel-case payment failed before acceptance".into());
    }

    let accepted = wait_for_accepted(
        &harness.alice,
        material.hash_commitment,
        PAYMENT_AMOUNT_SATS,
    );
    if let Err(error) = accepted {
        cancel_if_accepted(harness, material.hash_commitment);

        return Err(error);
    }

    harness.alice.cancel_hold(CancelHoldRequest {
        payment_hash: material.hash_commitment,
        deadline: Some(std::time::SystemTime::now() + OBSERVATION_TIMEOUT),
    })?;
    let canceled = wait_for_canceled(&harness.alice, material.hash_commitment)?;
    if canceled.state != InvoiceState::Canceled {
        return Err("real hold invoice did not reach CANCELED".into());
    }

    let tracked = wait_for_payment(&harness.bob, material.hash_commitment)?;
    if tracked.state != PaymentState::Failed || tracked.payment_preimage.is_some() {
        return Err("canceled Lightning payment did not finish without a preimage".into());
    }

    Ok(())
}

fn wait_for_accepted(
    client: &cross_network_market_maker::lightning::LndRestClient,
    payment_hash: [u8; 32],
    amount_sats: u64,
) -> Result<HoldInvoice, Box<dyn Error>> {
    wait_until(OBSERVATION_TIMEOUT, || {
        client.observe_accepted(payment_hash, amount_sats).ok()
    })
    .ok_or_else(|| "timed out waiting for an accepted hold invoice".into())
}

fn wait_for_canceled(
    client: &cross_network_market_maker::lightning::LndRestClient,
    payment_hash: [u8; 32],
) -> Result<HoldInvoice, Box<dyn Error>> {
    wait_until(OBSERVATION_TIMEOUT, || {
        client
            .lookup_invoice(payment_hash)
            .ok()
            .filter(|invoice| invoice.state == InvoiceState::Canceled)
    })
    .ok_or_else(|| "timed out waiting for a canceled hold invoice".into())
}

fn wait_for_payment(
    client: &cross_network_market_maker::lightning::LndRestClient,
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
    .ok_or_else(|| "timed out waiting for a terminal Lightning payment".into())
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

fn required_payment_request(invoice: &HoldInvoice) -> Result<String, Box<dyn Error>> {
    invoice
        .payment_request
        .clone()
        .ok_or_else(|| "LND hold invoice did not return a payment request".into())
}

fn cancel_if_accepted(harness: &RegtestSwapHarness, payment_hash: [u8; 32]) {
    let Ok(invoice) = harness.alice.lookup_invoice(payment_hash) else {
        return;
    };

    if invoice.state != InvoiceState::Accepted {
        return;
    }

    let _ = harness.alice.cancel_hold(CancelHoldRequest {
        payment_hash,
        deadline: Some(std::time::SystemTime::now() + OBSERVATION_TIMEOUT),
    });
}

fn require_live_flag(name: &str) -> Result<(), Box<dyn Error>> {
    if std::env::var(name).as_deref() != Ok("1") {
        return Err(format!("{name}=1 is required for this live test").into());
    }

    Ok(())
}
