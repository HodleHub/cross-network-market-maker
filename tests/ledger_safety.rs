use cross_network_market_maker::core::amount::CanonicalAmount;
use cross_network_market_maker::core::storage::{Reservation, ReservationState, SqliteStore};

fn amount(value: u64) -> CanonicalAmount {
    CanonicalAmount::parse(&value.to_string()).expect("canonical test amount")
}

fn reservation() -> Reservation {
    Reservation {
        reservation_id: "reservation-safety".to_owned(),
        solver_id: "maker-safety".to_owned(),
        quote_id: "quote-safety".to_owned(),
        swap_id: None,
        amount: amount(700),
        hash_commitment: "55".repeat(32),
        state: ReservationState::Held,
        expires_at: 200,
    }
}

#[test]
fn inventory_cannot_be_reserved_in_a_different_asset_or_beyond_capacity() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("maker.sqlite")).expect("store");
    store
        .ensure_solver_ledger("maker-safety", "BTC", &amount(1000))
        .expect("ledger");

    assert!(store.reserve(&reservation(), "TEST-DEPIX").is_err());
    store
        .reserve(&reservation(), "BTC")
        .expect("first reservation");

    let mut second = reservation();
    second.reservation_id = "second-reservation".to_owned();
    second.quote_id = "second-quote".to_owned();
    second.hash_commitment = "66".repeat(32);
    assert!(store.reserve(&second, "BTC").is_err());
    assert_eq!(
        store.get_ledger("maker-safety").expect("ledger"),
        Some((
            "BTC".to_owned(),
            "1000".to_owned(),
            "700".to_owned(),
            "0".to_owned()
        ))
    );
}

#[test]
fn attached_expired_reservation_stays_locked_without_local_swap_evidence() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("maker.sqlite");
    let mut store = SqliteStore::open(&path).expect("store");
    store
        .ensure_solver_ledger("maker-safety", "BTC", &amount(1000))
        .expect("ledger");
    store.reserve(&reservation(), "BTC").expect("reservation");
    store
        .attach_reservation("reservation-safety", "remote-client-swap", 100)
        .expect("attach");
    drop(store);
    let mut reopened = SqliteStore::open(&path).expect("reopen");

    assert!(
        reopened
            .release_reservation("reservation-safety", 300)
            .is_err()
    );
    assert_eq!(
        reopened.get_ledger("maker-safety").expect("ledger"),
        Some((
            "BTC".to_owned(),
            "1000".to_owned(),
            "700".to_owned(),
            "0".to_owned()
        ))
    );
}

#[test]
fn consuming_a_reservation_twice_debits_inventory_once() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("maker.sqlite")).expect("store");
    store
        .ensure_solver_ledger("maker-safety", "BTC", &amount(1000))
        .expect("ledger");
    store.reserve(&reservation(), "BTC").expect("reservation");
    store
        .consume_reservation("reservation-safety")
        .expect("consume");
    store
        .consume_reservation("reservation-safety")
        .expect("replay");

    assert_eq!(
        store.get_ledger("maker-safety").expect("ledger"),
        Some((
            "BTC".to_owned(),
            "300".to_owned(),
            "0".to_owned(),
            "700".to_owned()
        ))
    );
    assert!(
        store
            .release_reservation("reservation-safety", 300)
            .is_err()
    );
}

#[test]
fn ledger_keeps_large_integer_amounts_exact() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("maker.sqlite")).expect("store");
    store
        .ensure_solver_ledger("maker-safety", "BTC", &amount(u64::MAX))
        .expect("ledger");
    store.reserve(&reservation(), "BTC").expect("reservation");
    store
        .consume_reservation("reservation-safety")
        .expect("consume");

    assert_eq!(
        store.get_ledger("maker-safety").expect("ledger"),
        Some((
            "BTC".to_owned(),
            (u64::MAX - 700).to_string(),
            "0".to_owned(),
            "700".to_owned()
        ))
    );
}
