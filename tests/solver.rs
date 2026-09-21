use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use bitcoin::hashes::{Hash, sha256};
use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
use cross_network_market_maker::cli::solver::{
    ReserveRequest, RfqRequest, SolverConfig, SolverService, request_quote, reserve_remote,
};
use cross_network_market_maker::core::amount::CanonicalAmount;
use cross_network_market_maker::core::endpoint::EndpointId;
use cross_network_market_maker::core::hash::HashCommitment;
use cross_network_market_maker::core::network::Network;
use cross_network_market_maker::core::rfq::{
    DeadlineContext, Intent, PinnedSolverKeys, now_unix_seconds, verify_quote,
};
use cross_network_market_maker::core::routes::RouteRegistry;
use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
use tempfile::TempDir;

const TEST_ASSET_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn make_invoice(payment_hash: sha256::Hash, amount_sats: u64) -> String {
    let signer = SecretKey::from_slice(&[3_u8; 32]).expect("signer");
    let invoice = InvoiceBuilder::new(Currency::Regtest)
        .amount_milli_satoshis(amount_sats * 1000)
        .description("solver RFQ test".to_owned())
        .payment_hash(payment_hash)
        .payment_secret(PaymentSecret([5_u8; 32]))
        .min_final_cltv_expiry_delta(40)
        .current_timestamp()
        .build_signed(|message: &Message| Secp256k1::new().sign_ecdsa_recoverable(message, &signer))
        .expect("invoice");

    invoice.to_string()
}

fn make_intent(now: u64) -> Intent {
    let hash = sha256::Hash::from_slice(&[4_u8; 32]).expect("hash");
    Intent {
        version: 1,
        intent_id: "solver-http-intent".to_owned(),
        source: EndpointId::new(
            "TEST-DEPIX",
            Network::LiquidRegtest,
            Some(TEST_ASSET_HASH.to_owned()),
        )
        .expect("source"),
        destination: EndpointId::new("BTC", Network::LightningRegtest, None).expect("destination"),
        amount_in: CanonicalAmount::parse("1000").expect("input"),
        min_amount_out: CanonicalAmount::parse("1000").expect("minimum"),
        fee_limit_lbtc: CanonicalAmount::parse("20").expect("fee limit"),
        hash_commitment: HashCommitment::parse(&hex::encode(hash.to_byte_array())).expect("H"),
        client_claim_pubkey: "client-claim".to_owned(),
        client_refund_pubkey: "client-refund".to_owned(),
        source_destination: "liquid-destination".to_owned(),
        destination_destination: make_invoice(hash, 1000),
        refund_destination: "liquid-refund".to_owned(),
        deadline_context: DeadlineContext {
            source_height: 100,
            destination_height: 80,
            source_refund_height: 1200,
            destination_refund_height: 90,
            lightning_expiry_height: Some(100),
            source_refund_at: now + 20_000,
            destination_refund_at: now + 10_000,
            lightning_expiry_at: Some(now + 10_000),
            margin_seconds: 7_200,
        },
        created_at: now.saturating_sub(1),
        expires_at: now + 3_600,
        nonce: "solver-http-nonce".to_owned(),
    }
}

fn service(
    solver_id: &str,
    key_id: &str,
    key_byte: u8,
    fee: &str,
    database: &std::path::Path,
) -> SolverService {
    SolverService::new(SolverConfig {
        solver_id: solver_id.to_owned(),
        key_id: key_id.to_owned(),
        secret_key_hex: hex::encode([key_byte; 32]),
        asset_hash: TEST_ASSET_HASH.to_owned(),
        inventory_asset_id: "BTC".to_owned(),
        fee_lbtc: CanonicalAmount::parse(fee).expect("fee"),
        rate_numerator: 1,
        rate_denominator: 1,
        database: database.to_path_buf(),
        inventory_amount: CanonicalAmount::parse("1000000").expect("inventory"),
    })
    .expect("solver")
}

#[test]
fn two_independent_http_solvers_verify_and_select_the_lower_fee() {
    let directory = TempDir::new().expect("temp directory");
    let first_service = service(
        "solver-a",
        "key-a",
        11,
        "10",
        &directory.path().join("a.sqlite"),
    );
    let second_service = service(
        "solver-b",
        "key-b",
        12,
        "5",
        &directory.path().join("b.sqlite"),
    );
    let first_key = first_service.public_key_hex();
    let second_key = second_service.public_key_hex();
    let first_thread = thread::spawn(|| {
        cross_network_market_maker::cli::solver::serve("127.0.0.1:38171", first_service)
    });
    let second_thread = thread::spawn(|| {
        cross_network_market_maker::cli::solver::serve("127.0.0.1:38172", second_service)
    });
    let _threads = (first_thread, second_thread);
    wait_for_peer("127.0.0.1:38171");
    wait_for_peer("127.0.0.1:38172");

    let now = now_unix_seconds().expect("clock");
    let intent = make_intent(now);
    let request = RfqRequest {
        intent: intent.clone(),
    };
    let first = request_quote("127.0.0.1:38171", &request, 5).expect("first quote");
    let second = request_quote("127.0.0.1:38172", &request, 5).expect("second quote");
    let registry = RouteRegistry::qualified(TEST_ASSET_HASH).expect("registry");
    let mut keys = PinnedSolverKeys::default();
    keys.insert("key-a", first_key);
    keys.insert("key-b", second_key);
    verify_quote(
        &intent,
        &first.signed_quote,
        &keys,
        &registry,
        &intent.deadline_context,
        now,
    )
    .expect("first signature");
    verify_quote(
        &intent,
        &second.signed_quote,
        &keys,
        &registry,
        &intent.deadline_context,
        now,
    )
    .expect("second signature");
    assert_eq!(
        first.signed_quote.quote.amount_out,
        second.signed_quote.quote.amount_out
    );
    assert!(
        second.signed_quote.quote.fee_lbtc.value().expect("fee")
            < first.signed_quote.quote.fee_lbtc.value().expect("fee")
    );

    let reservation_id = "reservation-http-b".to_owned();
    let reserved = reserve_remote(
        "127.0.0.1:38172",
        &ReserveRequest {
            reservation_id: reservation_id.clone(),
            signed_quote: second.signed_quote,
            intent,
        },
        5,
    );
    assert!(reserved.is_ok());
}

fn wait_for_peer(address: &str) {
    for _ in 0..40 {
        if TcpStream::connect(address).is_ok() {
            return;
        }

        thread::sleep(Duration::from_millis(25));
    }

    panic!("solver peer did not bind: {address}");
}
