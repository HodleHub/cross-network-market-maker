use std::path::PathBuf;

use cross_network_market_maker::core::amount::CanonicalAmount;
use cross_network_market_maker::core::endpoint::EndpointId;
use cross_network_market_maker::core::hash::HashCommitment;
use cross_network_market_maker::core::network::Network;
use cross_network_market_maker::core::rfq::{
    DeadlineContext, Intent, PinnedSolverKeys, Quote, SignedQuote, sign_quote, verify_quote,
};
use cross_network_market_maker::core::routes::RouteRegistry;
use cross_network_market_maker::core::settlement::SwapState;
use cross_network_market_maker::core::storage::{
    Reservation, ReservationState, SqliteStore, SwapRecord, read_recovery_bundle,
    write_recovery_bundle,
};
use tempfile::TempDir;

const TEST_ASSET_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TEST_SECRET_KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";

fn test_intent(now: u64) -> Intent {
    Intent {
        version: 1,
        intent_id: "intent-core-1".to_owned(),
        source: EndpointId::new(
            "TEST-DEPIX",
            Network::LiquidRegtest,
            Some(TEST_ASSET_HASH.to_owned()),
        )
        .expect("source endpoint"),
        destination: EndpointId::new("BTC", Network::LightningRegtest, None)
            .expect("destination endpoint"),
        amount_in: CanonicalAmount::parse("1000").expect("amount"),
        min_amount_out: CanonicalAmount::parse("900").expect("minimum"),
        fee_limit_lbtc: CanonicalAmount::parse("50").expect("fee"),
        hash_commitment: HashCommitment::parse(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .expect("hash"),
        client_claim_pubkey: "02aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .to_owned(),
        client_refund_pubkey: "02bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .to_owned(),
        source_destination: "liquid-destination".to_owned(),
        destination_destination: "lightning-invoice-placeholder".to_owned(),
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
        nonce: "nonce-core-1".to_owned(),
    }
}

fn test_quote(intent: &Intent, now: u64) -> Quote {
    Quote {
        version: 1,
        quote_id: "quote-core-1".to_owned(),
        intent_id: intent.intent_id.clone(),
        source: intent.source.clone(),
        destination: intent.destination.clone(),
        amount_in: intent.amount_in.clone(),
        min_amount_out: intent.min_amount_out.clone(),
        amount_out: CanonicalAmount::parse("950").expect("amount out"),
        fee_lbtc: CanonicalAmount::parse("10").expect("fee"),
        fee_limit_lbtc: intent.fee_limit_lbtc.clone(),
        hash_commitment: intent.hash_commitment.clone(),
        client_claim_pubkey: intent.client_claim_pubkey.clone(),
        client_refund_pubkey: intent.client_refund_pubkey.clone(),
        source_destination: intent.source_destination.clone(),
        destination_destination: intent.destination_destination.clone(),
        refund_destination: intent.refund_destination.clone(),
        source_refund_height: intent.deadline_context.source_refund_height,
        destination_refund_height: intent.deadline_context.destination_refund_height,
        lightning_expiry_height: intent.deadline_context.lightning_expiry_height,
        deadline_context: intent.deadline_context.clone(),
        expires_at: now + 300,
        nonce: intent.nonce.clone(),
        solver_key_id: "solver-a".to_owned(),
        adapter_id: "lightning-hold".to_owned(),
        payment_request: None,
    }
}

fn test_record(intent: &Intent, quote: &Quote) -> SwapRecord {
    SwapRecord {
        swap_id: intent.intent_id.clone(),
        intent: intent.clone(),
        quote: SignedQuote {
            quote: quote.clone(),
            signature_hex: "00".repeat(64),
        },
        state: SwapState::Created,
        funding_txid: None,
        claim_txid: None,
        payment_hash: None,
        payment_evidence_json: None,
    }
}

#[test]
fn route_catalog_has_twenty_and_qualified_initial_routes() {
    let catalog = RouteRegistry::regtest().expect("catalog");
    assert_eq!(catalog.all().len(), 20);
    assert!(!catalog.is_enabled(
        &EndpointId::new("TEST-DEPIX", Network::LiquidRegtest, None).expect("source"),
        &EndpointId::new("BTC", Network::LightningRegtest, None).expect("destination"),
    ));

    let qualified = RouteRegistry::qualified(TEST_ASSET_HASH).expect("qualified");
    assert_eq!(qualified.all().len(), 20);
    assert!(
        qualified.is_enabled(
            &EndpointId::new(
                "TEST-DEPIX",
                Network::LiquidRegtest,
                Some(TEST_ASSET_HASH.to_owned()),
            )
            .expect("source"),
            &EndpointId::new("BTC", Network::LightningRegtest, None).expect("destination"),
        )
    );
}

#[test]
fn signed_quote_binds_intent_and_trusted_deadline_context() {
    let now = 1_700_000_000;
    let intent = test_intent(now);
    let quote = test_quote(&intent, now);
    let signed = sign_quote(&quote, TEST_SECRET_KEY).expect("signature");
    let secret_key = secp256k1::SecretKey::from_slice(&hex::decode(TEST_SECRET_KEY).expect("key"))
        .expect("secret");
    let public_key =
        secp256k1::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &secret_key);
    let mut pinned = PinnedSolverKeys::default();
    pinned.insert("solver-a", hex::encode(public_key.serialize()));
    let registry = RouteRegistry::qualified(TEST_ASSET_HASH).expect("registry");

    verify_quote(
        &intent,
        &signed,
        &pinned,
        &registry,
        &intent.deadline_context,
        now,
    )
    .expect("valid quote");

    let mut tampered = signed.clone();
    tampered.quote.amount_out = CanonicalAmount::parse("951").expect("tampered amount");
    assert!(
        verify_quote(
            &intent,
            &tampered,
            &pinned,
            &registry,
            &intent.deadline_context,
            now,
        )
        .is_err()
    );

    let mut hostile = signed;
    hostile.quote.source_refund_height += 1;
    assert!(
        verify_quote(
            &intent,
            &hostile,
            &pinned,
            &registry,
            &intent.deadline_context,
            now,
        )
        .is_err()
    );
}

#[test]
fn durable_reservation_and_unknown_state_survive_reopen() {
    let directory = TempDir::new().expect("temp directory");
    let database = directory.path().join("client.sqlite");
    let now = 1_700_000_000;
    let intent = test_intent(now);
    let quote = test_quote(&intent, now);
    let record = test_record(&intent, &quote);
    let mut store = SqliteStore::open(&database).expect("store");
    store.insert_swap(&record).expect("insert swap");
    store
        .ensure_solver_ledger(
            "solver-a",
            "BTC",
            &CanonicalAmount::parse("10000").expect("inventory"),
        )
        .expect("ledger");
    store
        .reserve(
            &Reservation {
                reservation_id: "reservation-core-1".to_owned(),
                solver_id: "solver-a".to_owned(),
                quote_id: quote.quote_id.clone(),
                swap_id: None,
                amount: CanonicalAmount::parse("950").expect("reservation amount"),
                hash_commitment: intent.hash_commitment.as_hex(),
                state: ReservationState::Held,
                expires_at: now + 10,
            },
            "BTC",
        )
        .expect("reserve");
    store
        .attach_reservation("reservation-core-1", &intent.intent_id, now)
        .expect("attach");
    store
        .transition_swap(
            &intent.intent_id,
            SwapState::Created,
            SwapState::InputFunding,
            None,
            None,
        )
        .expect("funding");
    store
        .transition_swap(
            &intent.intent_id,
            SwapState::InputFunding,
            SwapState::InputLocked,
            None,
            None,
        )
        .expect("locked");
    store
        .transition_swap(
            &intent.intent_id,
            SwapState::InputLocked,
            SwapState::OutputPending,
            None,
            None,
        )
        .expect("pending");
    store
        .transition_swap(
            &intent.intent_id,
            SwapState::OutputPending,
            SwapState::OutputUnknown,
            None,
            None,
        )
        .expect("unknown");
    assert!(
        store
            .release_reservation("reservation-core-1", now + 20)
            .is_err()
    );
    drop(store);

    let reopened = SqliteStore::open(&database).expect("reopen");
    assert_eq!(
        reopened
            .get_swap(&intent.intent_id)
            .expect("read")
            .expect("swap")
            .state,
        SwapState::OutputUnknown
    );
    assert_eq!(
        reopened
            .get_ledger("solver-a")
            .expect("ledger")
            .expect("row"),
        (
            "BTC".to_owned(),
            "10000".to_owned(),
            "950".to_owned(),
            "0".to_owned()
        )
    );
}

#[test]
fn recovery_bundle_is_encrypted_immutable_and_reopenable() {
    let directory = TempDir::new().expect("temp directory");
    let path: PathBuf = directory.path().join("recovery.bundle");
    let key = [7_u8; 32];
    let bundle = cross_network_market_maker::core::storage::RecoveryBundle {
        swap_id: "swap".to_owned(),
        hash_commitment: "ab".repeat(32),
        preimage_hex: "cd".repeat(32),
        client_claim_secret_hex: "ef".repeat(32),
        client_refund_secret_hex: "12".repeat(32),
        solver_key_id: "solver".to_owned(),
        reservation_id: "reservation".to_owned(),
        deterministic_funding_txid: "34".repeat(32),
    };
    write_recovery_bundle(&path, &key, &bundle).expect("write");
    assert!(write_recovery_bundle(&path, &key, &bundle).is_err());
    let bytes = std::fs::read(&path).expect("bytes");
    assert!(!String::from_utf8_lossy(&bytes).contains(&bundle.preimage_hex));
    assert_eq!(read_recovery_bundle(&path, &key).expect("read"), bundle);
}
