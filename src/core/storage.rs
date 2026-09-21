use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, OsRng, generic_array::GenericArray};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::amount::CanonicalAmount;
use super::error::{CoreError, CoreResult};
use super::rfq::{Intent, SignedQuote};
use super::settlement::{SwapState, validate_transition};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SwapRecord {
    pub swap_id: String,
    pub intent: Intent,
    pub quote: SignedQuote,
    pub state: SwapState,
    pub funding_txid: Option<String>,
    pub claim_txid: Option<String>,
    pub payment_hash: Option<String>,
    pub payment_evidence_json: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Reservation {
    pub reservation_id: String,
    pub solver_id: String,
    pub quote_id: String,
    pub swap_id: Option<String>,
    pub amount: CanonicalAmount,
    pub hash_commitment: String,
    pub state: ReservationState,
    pub expires_at: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReservationState {
    Held,
    Consumed,
    Released,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub operation_key: String,
    pub entity_id: String,
    pub request_digest: String,
    pub payload_json: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FundingOutbox {
    pub swap_id: String,
    pub deterministic_txid: String,
    pub request_json: String,
    pub status: String,
}

struct StoredSwap {
    intent_json: String,
    quote_json: String,
    state: String,
    funding_txid: Option<String>,
    claim_txid: Option<String>,
    payment_hash: Option<String>,
    payment_evidence_json: Option<String>,
}

pub struct SqliteStore {
    path: PathBuf,
    connection: Connection,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> CoreResult<Self> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| CoreError::Storage(error.to_string()))?;
        }

        let connection = Connection::open(&path).map_err(storage_error)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(10))
            .map_err(storage_error)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS swaps (
                   swap_id TEXT PRIMARY KEY,
                   intent_json TEXT NOT NULL,
                   quote_json TEXT NOT NULL,
                   state TEXT NOT NULL,
                   funding_txid TEXT,
                   claim_txid TEXT,
                   payment_hash TEXT,
                   payment_evidence_json TEXT,
                   request_digest TEXT NOT NULL,
                   created_at INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS reservations (
                   reservation_id TEXT PRIMARY KEY,
                   solver_id TEXT NOT NULL,
                   quote_id TEXT NOT NULL UNIQUE,
                   swap_id TEXT,
                   amount TEXT NOT NULL,
                   hash_commitment TEXT NOT NULL UNIQUE,
                   state TEXT NOT NULL,
                   expires_at INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS solver_ledgers (
                   solver_id TEXT PRIMARY KEY,
                   asset_id TEXT NOT NULL,
                   available_amount TEXT NOT NULL,
                   reserved_amount TEXT NOT NULL,
                   consumed_amount TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS journal (
                   operation_key TEXT PRIMARY KEY,
                   entity_id TEXT NOT NULL,
                   request_digest TEXT NOT NULL,
                   payload_json TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS idempotency (
                   operation_key TEXT PRIMARY KEY,
                   request_digest TEXT NOT NULL,
                   result_json TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS funding_outbox (
                   swap_id TEXT PRIMARY KEY,
                   deterministic_txid TEXT NOT NULL,
                   request_json TEXT NOT NULL,
                   status TEXT NOT NULL
                 );",
            )
            .map_err(storage_error)?;

        Ok(Self { path, connection })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn insert_swap(&mut self, record: &SwapRecord) -> CoreResult<()> {
        let intent_json = serde_json::to_string(&record.intent).map_err(serialization_error)?;
        let quote_json = serde_json::to_string(&record.quote).map_err(serialization_error)?;
        let request_digest = digest_text(&format!("{}:{intent_json}:{quote_json}", record.swap_id));
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let existing = transaction
            .query_row(
                "SELECT request_digest FROM swaps WHERE swap_id = ?1",
                params![record.swap_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(storage_error)?;

        if let Some(existing_digest) = existing {
            if existing_digest != request_digest {
                return Err(CoreError::Storage(
                    "swap replay payload does not match the original digest".to_owned(),
                ));
            }

            transaction.commit().map_err(storage_error)?;
            return Ok(());
        }

        transaction
            .execute(
                "INSERT INTO swaps
                 (swap_id, intent_json, quote_json, state, funding_txid, claim_txid,
                  payment_hash, payment_evidence_json, request_digest, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, strftime('%s','now'))",
                params![
                    record.swap_id,
                    intent_json,
                    quote_json,
                    state_text(record.state),
                    record.funding_txid,
                    record.claim_txid,
                    record.payment_hash,
                    record.payment_evidence_json,
                    request_digest,
                ],
            )
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)
    }

    pub fn get_swap(&self, swap_id: &str) -> CoreResult<Option<SwapRecord>> {
        self.connection
            .query_row(
                "SELECT intent_json, quote_json, state, funding_txid, claim_txid,
                 payment_hash, payment_evidence_json FROM swaps WHERE swap_id = ?1",
                params![swap_id],
                |row| {
                    let intent_json = row.get::<_, String>(0)?;
                    let quote_json = row.get::<_, String>(1)?;
                    let state = row.get::<_, String>(2)?;
                    Ok(StoredSwap {
                        intent_json,
                        quote_json,
                        state,
                        funding_txid: row.get(3)?,
                        claim_txid: row.get(4)?,
                        payment_hash: row.get(5)?,
                        payment_evidence_json: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(storage_error)?
            .map(|row| deserialize_swap(swap_id, row))
            .transpose()
    }

    pub fn transition_swap(
        &mut self,
        swap_id: &str,
        expected: SwapState,
        next: SwapState,
        payment_hash: Option<&str>,
        payment_evidence_json: Option<&str>,
    ) -> CoreResult<SwapRecord> {
        validate_transition(expected, next)?;
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let current = transaction
            .query_row(
                "SELECT state FROM swaps WHERE swap_id = ?1",
                params![swap_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(storage_error)?
            .ok_or_else(|| CoreError::Storage("swap does not exist".to_owned()))?;

        if current != state_text(expected) {
            return Err(CoreError::InvalidState(format!(
                "expected {expected:?}, found {current}"
            )));
        }

        transaction
            .execute(
                "UPDATE swaps SET state = ?2, payment_hash = COALESCE(?3, payment_hash),
                 payment_evidence_json = COALESCE(?4, payment_evidence_json)
                 WHERE swap_id = ?1 AND state = ?5",
                params![
                    swap_id,
                    state_text(next),
                    payment_hash,
                    payment_evidence_json,
                    state_text(expected),
                ],
            )
            .map_err(storage_error)?;
        append_journal_transaction(
            &transaction,
            &JournalEntry {
                operation_key: format!("swap-transition:{swap_id}:{current}->{:?}", next),
                entity_id: swap_id.to_owned(),
                request_digest: digest_text(&format!("{swap_id}:{current}:{next:?}")),
                payload_json: serde_json::json!({
                    "from": current,
                    "to": state_text(next),
                    "payment_hash": payment_hash,
                })
                .to_string(),
            },
        )?;
        transaction.commit().map_err(storage_error)?;

        self.get_swap(swap_id)?
            .ok_or_else(|| CoreError::Storage("swap disappeared after transition".to_owned()))
    }

    pub fn record_swap_evidence(
        &mut self,
        swap_id: &str,
        funding_txid: &str,
        claim_txid: &str,
        payment_hash: &str,
        payment_evidence_json: &str,
    ) -> CoreResult<SwapRecord> {
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let existing = transaction
            .query_row(
                "SELECT funding_txid, claim_txid, payment_hash, payment_evidence_json
                 FROM swaps WHERE swap_id = ?1",
                params![swap_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(storage_error)?
            .ok_or_else(|| CoreError::Storage("swap does not exist".to_owned()))?;

        if existing
            .0
            .as_deref()
            .is_some_and(|value| value != funding_txid)
            || existing
                .1
                .as_deref()
                .is_some_and(|value| value != claim_txid)
            || existing
                .2
                .as_deref()
                .is_some_and(|value| value != payment_hash)
            || existing
                .3
                .as_deref()
                .is_some_and(|value| value != payment_evidence_json)
        {
            return Err(CoreError::Storage(
                "swap evidence is immutable and does not match".to_owned(),
            ));
        }

        transaction
            .execute(
                "UPDATE swaps SET funding_txid = ?2, claim_txid = ?3, payment_hash = ?4,
                 payment_evidence_json = ?5 WHERE swap_id = ?1",
                params![
                    swap_id,
                    funding_txid,
                    claim_txid,
                    payment_hash,
                    payment_evidence_json,
                ],
            )
            .map_err(storage_error)?;
        append_journal_transaction(
            &transaction,
            &JournalEntry {
                operation_key: format!("swap-evidence:{swap_id}"),
                entity_id: swap_id.to_owned(),
                request_digest: digest_text(&format!(
                    "{swap_id}:{funding_txid}:{claim_txid}:{payment_hash}:{payment_evidence_json}"
                )),
                payload_json: serde_json::json!({
                    "funding_txid": funding_txid,
                    "claim_txid": claim_txid,
                    "payment_hash": payment_hash,
                    "payment_evidence": payment_evidence_json,
                })
                .to_string(),
            },
        )?;
        transaction.commit().map_err(storage_error)?;

        self.get_swap(swap_id)?
            .ok_or_else(|| CoreError::Storage("swap disappeared after evidence".to_owned()))
    }

    pub fn ensure_solver_ledger(
        &mut self,
        solver_id: &str,
        asset_id: &str,
        available_amount: &CanonicalAmount,
    ) -> CoreResult<()> {
        self.connection
            .execute(
                "INSERT OR IGNORE INTO solver_ledgers
                 (solver_id, asset_id, available_amount, reserved_amount, consumed_amount)
                 VALUES (?1, ?2, ?3, '0', '0')",
                params![solver_id, asset_id, available_amount.as_str()],
            )
            .map_err(storage_error)?;

        Ok(())
    }

    pub fn reserve(&mut self, reservation: &Reservation, asset_id: &str) -> CoreResult<()> {
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let ledger = read_ledger(&transaction, &reservation.solver_id)?;
        if ledger.0 != asset_id {
            return Err(CoreError::Storage(
                "reservation asset does not match solver ledger".to_owned(),
            ));
        }

        let available = parse_u64(&ledger.1)?;
        let reserved = parse_u64(&ledger.2)?;
        let amount = reservation.amount.value()?;
        let free = available
            .checked_sub(reserved)
            .ok_or_else(|| CoreError::Storage("ledger reserved exceeds available".to_owned()))?;

        if free < amount {
            return Err(CoreError::Storage(
                "solver inventory is insufficient".to_owned(),
            ));
        }

        transaction
            .execute(
                "INSERT INTO reservations
                 (reservation_id, solver_id, quote_id, swap_id, amount, hash_commitment,
                  state, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    reservation.reservation_id,
                    reservation.solver_id,
                    reservation.quote_id,
                    reservation.swap_id,
                    reservation.amount.as_str(),
                    reservation.hash_commitment,
                    reservation_state_text(reservation.state),
                    i64::try_from(reservation.expires_at).map_err(|_| CoreError::Storage(
                        "expiry exceeds SQLite range".to_owned()
                    ))?,
                ],
            )
            .map_err(storage_error)?;
        transaction
            .execute(
                "UPDATE solver_ledgers SET reserved_amount = ?2 WHERE solver_id = ?1",
                params![
                    reservation.solver_id,
                    reserved
                        .checked_add(amount)
                        .ok_or_else(|| CoreError::Storage("reserved amount overflow".to_owned()))?
                        .to_string(),
                ],
            )
            .map_err(storage_error)?;
        append_journal_transaction(
            &transaction,
            &JournalEntry {
                operation_key: format!("reserve:{}", reservation.reservation_id),
                entity_id: reservation.reservation_id.clone(),
                request_digest: digest_text(
                    &serde_json::to_string(reservation).map_err(serialization_error)?,
                ),
                payload_json: serde_json::to_string(reservation).map_err(serialization_error)?,
            },
        )?;
        transaction.commit().map_err(storage_error)
    }

    pub fn attach_reservation(
        &mut self,
        reservation_id: &str,
        swap_id: &str,
        now: u64,
    ) -> CoreResult<()> {
        let reservation = self
            .connection
            .query_row(
                "SELECT state, expires_at FROM reservations WHERE reservation_id = ?1",
                params![reservation_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(storage_error)?
            .ok_or_else(|| CoreError::Storage("reservation does not exist".to_owned()))?;

        let current_swap = self
            .connection
            .query_row(
                "SELECT swap_id FROM reservations WHERE reservation_id = ?1",
                params![reservation_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(storage_error)?;

        if current_swap.flatten().as_deref() == Some(swap_id) {
            return Ok(());
        }

        if reservation.0 != reservation_state_text(ReservationState::Held)
            || u64::try_from(reservation.1).unwrap_or_default() <= now
        {
            return Err(CoreError::Storage(
                "only an unexpired held reservation can attach".to_owned(),
            ));
        }

        let changed = self
            .connection
            .execute(
                "UPDATE reservations SET swap_id = ?2 WHERE reservation_id = ?1 AND swap_id IS NULL",
                params![reservation_id, swap_id],
            )
            .map_err(storage_error)?;

        if changed == 1 {
            return Ok(());
        }

        let current = self
            .connection
            .query_row(
                "SELECT swap_id FROM reservations WHERE reservation_id = ?1",
                params![reservation_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(storage_error)?;

        if current.flatten().as_deref() == Some(swap_id) {
            return Ok(());
        }

        Err(CoreError::Storage(
            "reservation is missing or attached to another swap".to_owned(),
        ))
    }

    pub fn consume_reservation(&mut self, reservation_id: &str) -> CoreResult<()> {
        self.consume_reservation_with_hash(reservation_id, None)
    }

    pub fn get_reservation(&self, reservation_id: &str) -> CoreResult<Option<Reservation>> {
        self.connection
            .query_row(
                "SELECT solver_id, quote_id, swap_id, amount, hash_commitment, state, expires_at
                 FROM reservations WHERE reservation_id = ?1",
                params![reservation_id],
                |row| {
                    let amount =
                        CanonicalAmount::parse(&row.get::<_, String>(3)?).map_err(|error| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                        })?;
                    let expires_at = u64::try_from(row.get::<_, i64>(6)?).map_err(|error| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                    })?;
                    let state =
                        parse_reservation_state(&row.get::<_, String>(5)?).map_err(|error| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                        })?;
                    Ok(Reservation {
                        reservation_id: reservation_id.to_owned(),
                        solver_id: row.get(0)?,
                        quote_id: row.get(1)?,
                        swap_id: row.get(2)?,
                        amount,
                        hash_commitment: row.get(4)?,
                        state,
                        expires_at,
                    })
                },
            )
            .optional()
            .map_err(storage_error)
    }

    pub fn consume_reservation_verified(
        &mut self,
        reservation_id: &str,
        preimage: &[u8; 32],
    ) -> CoreResult<()> {
        let expected_hash = hex::encode(Sha256::digest(preimage));

        self.consume_reservation_with_hash(reservation_id, Some(expected_hash.as_str()))
    }

    fn consume_reservation_with_hash(
        &mut self,
        reservation_id: &str,
        expected_hash: Option<&str>,
    ) -> CoreResult<()> {
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let row = transaction
            .query_row(
                "SELECT solver_id, amount, state, hash_commitment FROM reservations
                 WHERE reservation_id = ?1",
                params![reservation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(storage_error)?
            .ok_or_else(|| CoreError::Storage("reservation does not exist".to_owned()))?;

        if let Some(expected_hash) = expected_hash
            && row.3 != expected_hash
        {
            return Err(CoreError::Storage(
                "preimage does not match the reservation commitment".to_owned(),
            ));
        }

        if row.2 == reservation_state_text(ReservationState::Consumed) {
            transaction.commit().map_err(storage_error)?;
            return Ok(());
        }

        if row.2 != reservation_state_text(ReservationState::Held) {
            return Err(CoreError::Storage("reservation is not held".to_owned()));
        }

        let ledger = read_ledger(&transaction, &row.0)?;
        let available = parse_u64(&ledger.1)?;
        let reserved = parse_u64(&ledger.2)?;
        let consumed = parse_u64(&ledger.3)?;
        let amount = row
            .1
            .parse::<u64>()
            .map_err(|_| CoreError::Storage("invalid ledger amount".to_owned()))?;

        if available < amount || reserved < amount {
            return Err(CoreError::Storage(
                "ledger balance assertion failed".to_owned(),
            ));
        }

        transaction
            .execute(
                "UPDATE reservations SET state = ?2 WHERE reservation_id = ?1 AND state = ?3",
                params![
                    reservation_id,
                    reservation_state_text(ReservationState::Consumed),
                    reservation_state_text(ReservationState::Held),
                ],
            )
            .map_err(storage_error)?;
        transaction
            .execute(
                "UPDATE solver_ledgers SET available_amount = ?2, reserved_amount = ?3,
                 consumed_amount = ?4 WHERE solver_id = ?1",
                params![
                    row.0,
                    available
                        .checked_sub(amount)
                        .ok_or_else(|| CoreError::Storage(
                            "available balance underflow".to_owned()
                        ))?
                        .to_string(),
                    reserved
                        .checked_sub(amount)
                        .ok_or_else(|| CoreError::Storage("reserved balance underflow".to_owned()))?
                        .to_string(),
                    consumed
                        .checked_add(amount)
                        .ok_or_else(|| CoreError::Storage("consumed amount overflow".to_owned()))?
                        .to_string(),
                ],
            )
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)
    }

    pub fn release_reservation(&mut self, reservation_id: &str, now: u64) -> CoreResult<()> {
        let transaction = self.connection.transaction().map_err(storage_error)?;
        let row = transaction
            .query_row(
                "SELECT solver_id, amount, state, swap_id, expires_at FROM reservations
                 WHERE reservation_id = ?1",
                params![reservation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(storage_error)?
            .ok_or_else(|| CoreError::Storage("reservation does not exist".to_owned()))?;

        if row.2 == reservation_state_text(ReservationState::Released) {
            transaction.commit().map_err(storage_error)?;
            return Ok(());
        }

        if row.2 != reservation_state_text(ReservationState::Held) {
            return Err(CoreError::Storage(
                "consumed reservation cannot release".to_owned(),
            ));
        }

        if u64::try_from(row.4).unwrap_or_default() > now {
            return Err(CoreError::Storage("reservation has not expired".to_owned()));
        }

        if let Some(swap_id) = row.3 {
            let state = transaction
                .query_row(
                    "SELECT state FROM swaps WHERE swap_id = ?1",
                    params![swap_id],
                    |query| query.get::<_, String>(0),
                )
                .optional()
                .map_err(storage_error)?;

            let state = state.ok_or_else(|| {
                CoreError::Storage("attached reservation has no matching swap".to_owned())
            })?;

            if !is_releaseable_state(&state) {
                return Err(CoreError::Storage(
                    "exposed swap reservation cannot release".to_owned(),
                ));
            }
        }

        let ledger = read_ledger(&transaction, &row.0)?;
        let reserved = parse_u64(&ledger.2)?;
        let amount = row
            .1
            .parse::<u64>()
            .map_err(|_| CoreError::Storage("invalid ledger amount".to_owned()))?;

        if reserved < amount {
            return Err(CoreError::Storage(
                "ledger reserved balance assertion failed".to_owned(),
            ));
        }

        transaction
            .execute(
                "UPDATE reservations SET state = ?2 WHERE reservation_id = ?1 AND state = ?3",
                params![
                    reservation_id,
                    reservation_state_text(ReservationState::Released),
                    reservation_state_text(ReservationState::Held),
                ],
            )
            .map_err(storage_error)?;
        transaction
            .execute(
                "UPDATE solver_ledgers SET reserved_amount = ?2 WHERE solver_id = ?1",
                params![
                    row.0,
                    reserved
                        .checked_sub(amount)
                        .ok_or_else(|| CoreError::Storage("reserved balance underflow".to_owned()))?
                        .to_string(),
                ],
            )
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)
    }

    pub fn append_journal(&mut self, entry: &JournalEntry) -> CoreResult<()> {
        let transaction = self.connection.transaction().map_err(storage_error)?;
        append_journal_transaction(&transaction, entry)?;
        transaction.commit().map_err(storage_error)
    }

    pub fn save_outbox(&mut self, outbox: &FundingOutbox) -> CoreResult<()> {
        self.connection
            .execute(
                "INSERT INTO funding_outbox (swap_id, deterministic_txid, request_json, status)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(swap_id) DO UPDATE SET deterministic_txid = excluded.deterministic_txid,
                 request_json = excluded.request_json, status = excluded.status",
                params![
                    outbox.swap_id,
                    outbox.deterministic_txid,
                    outbox.request_json,
                    outbox.status,
                ],
            )
            .map_err(storage_error)?;

        Ok(())
    }

    pub fn get_outbox(&self, swap_id: &str) -> CoreResult<Option<FundingOutbox>> {
        self.connection
            .query_row(
                "SELECT deterministic_txid, request_json, status FROM funding_outbox WHERE swap_id = ?1",
                params![swap_id],
                |row| {
                    Ok(FundingOutbox {
                        swap_id: swap_id.to_owned(),
                        deterministic_txid: row.get(0)?,
                        request_json: row.get(1)?,
                        status: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(storage_error)
    }

    pub fn get_ledger(
        &self,
        solver_id: &str,
    ) -> CoreResult<Option<(String, String, String, String)>> {
        self.connection
            .query_row(
                "SELECT asset_id, available_amount, reserved_amount, consumed_amount
                 FROM solver_ledgers WHERE solver_id = ?1",
                params![solver_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(storage_error)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoveryBundle {
    pub swap_id: String,
    pub hash_commitment: String,
    pub preimage_hex: String,
    pub client_claim_secret_hex: String,
    pub client_refund_secret_hex: String,
    pub solver_key_id: String,
    pub reservation_id: String,
    pub deterministic_funding_txid: String,
}

impl std::fmt::Debug for RecoveryBundle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecoveryBundle")
            .field("swap_id", &self.swap_id)
            .field("hash_commitment", &self.hash_commitment)
            .field("solver_key_id", &self.solver_key_id)
            .field("reservation_id", &self.reservation_id)
            .field(
                "deterministic_funding_txid",
                &self.deterministic_funding_txid,
            )
            .field("secrets", &"[redacted]")
            .finish()
    }
}

pub fn write_recovery_bundle(
    path: impl AsRef<Path>,
    encryption_key: &[u8; 32],
    bundle: &RecoveryBundle,
) -> CoreResult<()> {
    let plaintext = serde_json::to_vec(bundle).map_err(serialization_error)?;
    let cipher = Aes256Gcm::new(GenericArray::from_slice(encryption_key));
    let mut nonce_bytes = [0_u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = GenericArray::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_ref())
        .map_err(|error| CoreError::Storage(error.to_string()))?;
    let mut output = nonce_bytes.to_vec();
    output.extend_from_slice(&ciphertext);
    let target = path.as_ref();

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(storage_error)?;
    }

    if target.exists() {
        return Err(CoreError::Storage(
            "recovery bundle already exists; refusing overwrite".to_owned(),
        ));
    }

    let mut options = OpenOptions::new();
    options.create_new(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(target).map_err(storage_error)?;
    use std::io::Write;
    file.write_all(&output).map_err(storage_error)?;
    file.sync_all().map_err(storage_error)?;
    set_mode_600(target)?;

    Ok(())
}

pub fn read_recovery_bundle(
    path: impl AsRef<Path>,
    encryption_key: &[u8; 32],
) -> CoreResult<RecoveryBundle> {
    let bytes = fs::read(path).map_err(storage_error)?;

    if bytes.len() < 12 {
        return Err(CoreError::Storage(
            "recovery bundle is truncated".to_owned(),
        ));
    }

    let cipher = Aes256Gcm::new(GenericArray::from_slice(encryption_key));
    let nonce = GenericArray::from_slice(&bytes[..12]);
    let plaintext = cipher
        .decrypt(nonce, &bytes[12..])
        .map_err(|error| CoreError::Storage(error.to_string()))?;

    serde_json::from_slice(&plaintext).map_err(serialization_error)
}

fn deserialize_swap(swap_id: &str, row: StoredSwap) -> CoreResult<SwapRecord> {
    Ok(SwapRecord {
        swap_id: swap_id.to_owned(),
        intent: serde_json::from_str(&row.intent_json).map_err(serialization_error)?,
        quote: serde_json::from_str(&row.quote_json).map_err(serialization_error)?,
        state: parse_state(&row.state)?,
        funding_txid: row.funding_txid,
        claim_txid: row.claim_txid,
        payment_hash: row.payment_hash,
        payment_evidence_json: row.payment_evidence_json,
    })
}

fn read_ledger(
    transaction: &Transaction<'_>,
    solver_id: &str,
) -> CoreResult<(String, String, String, String)> {
    transaction
        .query_row(
            "SELECT asset_id, available_amount, reserved_amount, consumed_amount
             FROM solver_ledgers WHERE solver_id = ?1",
            params![solver_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(storage_error)
}

fn append_journal_transaction(
    transaction: &Transaction<'_>,
    entry: &JournalEntry,
) -> CoreResult<()> {
    let current = transaction
        .query_row(
            "SELECT entity_id, request_digest, payload_json FROM journal WHERE operation_key = ?1",
            params![entry.operation_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(storage_error)?;

    if let Some((entity_id, request_digest, payload_json)) = current {
        if entity_id != entry.entity_id
            || request_digest != entry.request_digest
            || payload_json != entry.payload_json
        {
            return Err(CoreError::Storage(
                "journal operation key reused with different payload".to_owned(),
            ));
        }

        return Ok(());
    }

    transaction
        .execute(
            "INSERT INTO journal (operation_key, entity_id, request_digest, payload_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                entry.operation_key,
                entry.entity_id,
                entry.request_digest,
                entry.payload_json,
            ],
        )
        .map_err(storage_error)?;

    Ok(())
}

fn state_text(state: SwapState) -> &'static str {
    match state {
        SwapState::Created => "CREATED",
        SwapState::InputFunding => "INPUT_FUNDING",
        SwapState::InputLocked => "INPUT_LOCKED",
        SwapState::OutputPending => "OUTPUT_PENDING",
        SwapState::OutputUnknown => "OUTPUT_UNKNOWN",
        SwapState::OutputSettled => "OUTPUT_SETTLED",
        SwapState::ClaimPending => "CLAIM_PENDING",
        SwapState::Settled => "SETTLED",
        SwapState::Refunding => "REFUNDING",
        SwapState::Refunded => "REFUNDED",
        SwapState::Failed => "FAILED",
    }
}

fn parse_state(value: &str) -> CoreResult<SwapState> {
    match value {
        "CREATED" => Ok(SwapState::Created),
        "INPUT_FUNDING" => Ok(SwapState::InputFunding),
        "INPUT_LOCKED" => Ok(SwapState::InputLocked),
        "OUTPUT_PENDING" => Ok(SwapState::OutputPending),
        "OUTPUT_UNKNOWN" => Ok(SwapState::OutputUnknown),
        "OUTPUT_SETTLED" => Ok(SwapState::OutputSettled),
        "CLAIM_PENDING" => Ok(SwapState::ClaimPending),
        "SETTLED" => Ok(SwapState::Settled),
        "REFUNDING" => Ok(SwapState::Refunding),
        "REFUNDED" => Ok(SwapState::Refunded),
        "FAILED" => Ok(SwapState::Failed),
        other => Err(CoreError::Storage(format!("unknown swap state {other}"))),
    }
}

fn reservation_state_text(state: ReservationState) -> &'static str {
    match state {
        ReservationState::Held => "HELD",
        ReservationState::Consumed => "CONSUMED",
        ReservationState::Released => "RELEASED",
    }
}

fn parse_reservation_state(value: &str) -> CoreResult<ReservationState> {
    match value {
        "HELD" => Ok(ReservationState::Held),
        "CONSUMED" => Ok(ReservationState::Consumed),
        "RELEASED" => Ok(ReservationState::Released),
        other => Err(CoreError::Storage(format!(
            "unknown reservation state {other}"
        ))),
    }
}

fn is_releaseable_state(value: &str) -> bool {
    matches!(value, "CREATED" | "FAILED" | "REFUNDED")
}

fn parse_u64(value: &str) -> CoreResult<u64> {
    value
        .parse::<u64>()
        .map_err(|_| CoreError::Storage(format!("invalid ledger amount {value}")))
}

fn digest_text(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn storage_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::Storage(error.to_string())
}

fn serialization_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::Serialization(error.to_string())
}

fn set_mode_600(path: &Path) -> CoreResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).map_err(storage_error)?.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions).map_err(storage_error)?;
    }

    Ok(())
}
