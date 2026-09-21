use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rand::RngCore;
use serde::Serialize;

use super::commands::{
    PrepareArgs, SelectedQuoteOutput, SwapArgs, parse_pinned_keys, print_json, read_json,
    validate_quote_payment_request, write_json,
};
use crate::chain::{AssetId, Chain};
use crate::core::error::{CoreError, CoreResult};
use crate::core::hash::HashCommitment;
use crate::core::network::Network;
use crate::core::rfq::{DeadlineContext, Intent, now_unix_seconds, verify_quote};
use crate::core::routes::RouteRegistry;
use crate::core::settlement::SwapState;
use crate::core::storage::{SqliteStore, SwapRecord};
use crate::lightning::{
    HoldInvoiceRequest, InvoiceState, LightningNetwork, LndRestClient, LndRestConfig,
};
use crate::rpc::RpcClient;
use crate::swaps::{
    ForwardSwapRequest, LiquidRpcAdapter, PrepareRecoveryRequest, SwapDirection, SwapEncryptionKey,
    SwapError, SwapKeyMaterial, SwapObserver, SwapPaths, SwapPhase, SwapTerms,
    prepare_or_load_recovery, read_recovery, run_forward_swap,
};

const DEFAULT_POLICY_ASSET_ID: &str =
    "b2e15d0d7a0c94e4e2ce0fe6e8691b9e451377f6e46e8045a86f7c4b5d4f0f23";
const REGTEST_LIQUID_BLOCK_SECONDS: u64 = 60;
const REGTEST_BITCOIN_BLOCK_SECONDS: u64 = 600;
const DEFAULT_SESSION_DIR: &str = "runtime/sessions";

pub fn run_prepare(args: PrepareArgs) -> CoreResult<()> {
    validate_session_id(&args.session)?;
    let amount_in = parse_amount(&args.amount_in)?;
    let amount_out = parse_amount(&args.amount_out)?;
    let fee_limit = crate::core::amount::CanonicalAmount::parse(&args.fee_limit_lbtc)?;
    let asset_id = AssetId::from_hex(&args.asset_hash).map_err(chain_error)?;
    let fee_asset_id = AssetId::from_hex(DEFAULT_POLICY_ASSET_ID).map_err(chain_error)?;
    let recovery_key = load_or_create_key(&args.recovery_key)?;
    let paths = session_paths(&args.recovery_key, &args.session);
    let elements = rpc_client(
        &args.elements_rpc,
        &args.elements_user,
        &args.elements_password,
    )?;
    let source_height = elements_height(&elements)?;
    let receiver = lnd_client(
        &args.receiver_url,
        &args.receiver_cert,
        &args.receiver_macaroon,
    )?;
    let lightning_tip = receiver
        .get_info()
        .map_err(|error| CoreError::Adapter(error.to_string()))?;
    let now = now_unix_seconds()?;
    let source_refund_height = source_height
        .checked_add(args.source_refund_delta)
        .ok_or_else(|| CoreError::DeadlineRejected("source refund height overflows".to_owned()))?;
    let lightning_expiry_height = lightning_tip
        .block_height
        .checked_add(args.lightning_max_cltv)
        .ok_or_else(|| {
            CoreError::DeadlineRejected("Lightning expiry height overflows".to_owned())
        })?;
    let source_refund_at = now
        .checked_add(
            args.source_refund_delta
                .saturating_mul(REGTEST_LIQUID_BLOCK_SECONDS),
        )
        .ok_or_else(|| CoreError::DeadlineRejected("source refund time overflows".to_owned()))?;
    let lightning_expiry_at = now
        .checked_add(
            args.lightning_max_cltv
                .saturating_mul(REGTEST_BITCOIN_BLOCK_SECONDS),
        )
        .ok_or_else(|| CoreError::DeadlineRejected("Lightning expiry time overflows".to_owned()))?;

    if source_refund_at <= lightning_expiry_at.saturating_add(args.margin_seconds) {
        return Err(CoreError::DeadlineRejected(
            "source refund does not leave the Lightning safety margin".to_owned(),
        ));
    }

    let terms = PrepareRecoveryRequest {
        path: &paths.recovery,
        encryption_key: &recovery_key,
        direction: SwapDirection::DepixToLightning,
        chain: Chain::LiquidRegtest,
        asset_id,
        fee_asset_id,
        amount_sats: amount_out,
        asset_amount_sats: amount_in,
        fee_sats: args.fee_sats,
        refund_lock_height: source_refund_height,
        quote_id: None,
        supplied_material: None,
    };
    let (record, _) = prepare_or_load_recovery(terms).map_err(swap_error)?;
    let refund_destination = derived_refund_destination(&record.session.material);

    if args
        .refund_destination
        .as_deref()
        .is_some_and(|value| value != refund_destination)
    {
        return Err(CoreError::QuoteRejected(
            "refund destination does not match the prepared destination key".to_owned(),
        ));
    }

    let invoice = receiver
        .lookup_invoice(record.session.material.hash_commitment)
        .or_else(|_| {
            receiver.create_hold_invoice(HoldInvoiceRequest {
                payment_hash: record.session.material.hash_commitment,
                amount_sats: amount_out,
                cltv_expiry: args.hold_cltv,
                memo: Some(args.session.clone()),
            })
        })
        .map_err(|error| CoreError::Adapter(error.to_string()))?;
    let payment_request = invoice.payment_request.clone().ok_or_else(|| {
        CoreError::Adapter("LND did not return a BOLT11 payment request".to_owned())
    })?;

    if invoice.payment_hash != record.session.material.hash_commitment
        || invoice.amount_sats != amount_out
        || invoice.state == InvoiceState::Canceled
    {
        return Err(CoreError::Adapter(
            "prepared hold invoice does not match the recovery session".to_owned(),
        ));
    }
    let deadline_context = DeadlineContext {
        source_height,
        destination_height: lightning_tip.block_height,
        source_refund_height,
        destination_refund_height: lightning_expiry_height,
        lightning_expiry_height: Some(lightning_expiry_height),
        source_refund_at,
        destination_refund_at: lightning_expiry_at,
        lightning_expiry_at: Some(lightning_expiry_at),
        margin_seconds: args.margin_seconds,
    };
    let intent = build_intent(BuildIntentRequest {
        args: &args,
        material: &record.session.material,
        amount_in,
        amount_out,
        fee_limit_lbtc: fee_limit,
        deadline_context,
        payment_request,
        now,
        source_destination: &record.contract.address,
        refund_destination: &refund_destination,
    })?;
    if args.intent_out.exists() {
        let existing: Intent = read_json(&args.intent_out)?;

        if existing != intent {
            return Err(CoreError::Storage(
                "public intent path already contains different terms".to_owned(),
            ));
        }
    } else {
        write_json(&args.intent_out, &intent)?;
    }

    print_json(&PreparedOutput {
        session: args.session,
        intent_id: intent.intent_id,
        hash_commitment: intent.hash_commitment.as_hex(),
        payment_request: intent.destination_destination,
        recovery_path: paths.recovery.display().to_string(),
        intent_path: args.intent_out.display().to_string(),
    })
}

pub fn run_forward(args: SwapArgs, resume: bool) -> CoreResult<()> {
    let intent: Intent = read_json(&args.intent)?;
    let selected: SelectedQuoteOutput = read_json(&args.quote)?;
    let existing = if resume {
        SqliteStore::open(&args.database)?.get_swap(&intent.intent_id)?
    } else {
        None
    };
    validate_forward_inputs(&args, &intent, &selected, resume, existing.as_ref())?;
    let recovery_key = read_key(&args.recovery_key)?;
    let paths = session_paths(&args.recovery_key, &intent.intent_id);
    let recovery = read_recovery(&paths.recovery, &recovery_key)
        .map_err(swap_error)?
        .ok_or_else(|| CoreError::Storage("swap prepare record is missing".to_owned()))?;
    validate_recovery_binding(&args, &intent, &selected, &recovery)?;
    let fee_asset_id = AssetId::from_hex(DEFAULT_POLICY_ASSET_ID).map_err(chain_error)?;
    let terms = swap_terms(&intent, &selected, &args, fee_asset_id)?;
    let mut store = SqliteStore::open(&args.database)?;
    ensure_swap_record(&mut store, &intent, &selected.signed_quote, resume)?;
    let solver_database = args
        .solver_database
        .clone()
        .ok_or_else(|| CoreError::Storage("solver database is required".to_owned()))?;
    prepare_solver_reservation(
        &solver_database,
        &intent,
        &selected,
        resume,
        existing.as_ref().map(|record| record.state),
    )?;
    let observer = CoreSwapObserver::new(
        store,
        intent.intent_id.clone(),
        args.stop_after_payment,
        solver_database.clone(),
        selected.reservation_id.clone(),
        recovery.session.material.preimage,
    );
    let elements_rpc = rpc_client(
        &args.elements_rpc,
        &args.elements_user,
        &args.elements_password,
    )?;
    let chain = LiquidRpcAdapter::new(elements_rpc, args.elements_wallet.clone(), fee_asset_id)
        .map_err(swap_error)?;
    let receiver = lnd_client(
        &args.receiver_url,
        &args.receiver_cert,
        &args.receiver_macaroon,
    )?;
    let payer = lnd_client(&args.payer_url, &args.payer_cert, &args.payer_macaroon)?;
    let evidence = run_forward_swap(ForwardSwapRequest {
        chain: &chain,
        receiver: &receiver,
        payer: &payer,
        paths,
        encryption_key: SwapEncryptionKey(recovery_key),
        terms,
        supplied_material: Some(recovery.session.material.clone()),
        memo: Some(intent.intent_id.clone()),
        observer: &observer,
    })
    .map_err(swap_error)?;
    let payment_hash = intent.hash_commitment.as_hex();
    let public_evidence = serde_json::to_string(&PublicEvidence::from(&evidence))
        .map_err(|error| CoreError::Serialization(error.to_string()))?;
    observer.record_evidence(
        &evidence.funding_txid,
        &evidence.claim_txid,
        &payment_hash,
        &public_evidence,
    )?;
    let solver_reservation_consumed =
        reservation_is_consumed(&solver_database, &selected.reservation_id)?;

    if !solver_reservation_consumed {
        return Err(CoreError::Storage(
            "settled swap has not consumed its solver reservation".to_owned(),
        ));
    }

    print_json(&LiveOutput {
        session_id: intent.intent_id,
        state: "SETTLED".to_owned(),
        funding_txid: evidence.funding_txid,
        claim_txid: evidence.claim_txid,
        accepted_expiry_height: evidence.accepted_expiry_height,
        preimage_verified: evidence.preimage_verified,
        solver_reservation_consumed,
        replay: resume,
    })
}

fn validate_forward_inputs(
    args: &SwapArgs,
    intent: &Intent,
    selected: &SelectedQuoteOutput,
    resume: bool,
    existing: Option<&SwapRecord>,
) -> CoreResult<()> {
    if selected.quote_id != selected.signed_quote.quote.quote_id
        || selected.amount_in != selected.signed_quote.quote.amount_in.to_string()
        || selected.amount_out != selected.signed_quote.quote.amount_out.to_string()
        || selected.fee_lbtc != selected.signed_quote.quote.fee_lbtc.to_string()
        || selected.expires_at != selected.signed_quote.quote.expires_at
    {
        return Err(CoreError::QuoteRejected(
            "selected quote envelope does not match its signed quote".to_owned(),
        ));
    }

    if intent.source.asset_hash.as_deref() != Some(args.asset_hash.as_str()) {
        return Err(CoreError::QuoteRejected(
            "intent asset hash differs from the selected fixture".to_owned(),
        ));
    }

    if args.solver_database.is_none() {
        return Err(CoreError::Storage(
            "--solver-database is required before any payment side effect".to_owned(),
        ));
    }

    if resume && !args.database.exists() {
        return Err(CoreError::Storage(
            "resume database does not exist".to_owned(),
        ));
    }

    if let Some(record) = existing
        && matches!(
            record.state,
            SwapState::Refunding | SwapState::Refunded | SwapState::Failed
        )
    {
        return Err(CoreError::InvalidState(
            "a refund or failed session cannot be resumed as a forward swap".to_owned(),
        ));
    }

    let registry = RouteRegistry::qualified(&args.asset_hash)?;
    let pinned_keys = parse_pinned_keys(&args.pinned_key)?;
    let verification_now = if resume
        && existing.is_some_and(|record| state_rank(record.state) > state_rank(SwapState::Created))
    {
        selected.signed_quote.quote.expires_at.saturating_sub(1)
    } else {
        now_unix_seconds()?
    };
    verify_quote(
        intent,
        &selected.signed_quote,
        &pinned_keys,
        &registry,
        &intent.deadline_context,
        verification_now,
    )?;
    validate_quote_payment_request(intent, &selected.signed_quote)
}

fn validate_recovery_binding(
    args: &SwapArgs,
    intent: &Intent,
    selected: &SelectedQuoteOutput,
    recovery: &crate::swaps::SwapRecoveryRecord,
) -> CoreResult<()> {
    let session = &recovery.session;
    let asset_id = AssetId::from_hex(&args.asset_hash).map_err(chain_error)?;
    let fee_asset_id = AssetId::from_hex(DEFAULT_POLICY_ASSET_ID).map_err(chain_error)?;

    if session.material.hash_commitment != intent.hash_commitment.bytes()
        || session.amount_sats != selected.signed_quote.quote.amount_out.value()?
        || session.asset_amount_sats != selected.signed_quote.quote.amount_in.value()?
        || session.asset_id != asset_id
        || session.fee_asset_id != fee_asset_id
        || session.fee_sats != args.fee_sats
        || session.refund_lock_height != intent.deadline_context.source_refund_height
        || recovery.contract.hash_lock != intent.hash_commitment.bytes()
        || recovery.contract.claim_pubkey.serialize().to_vec()
            != hex::decode(&intent.client_claim_pubkey).map_err(|_| {
                CoreError::QuoteRejected("client claim key is not hexadecimal".to_owned())
            })?
        || recovery.contract.refund_pubkey.serialize().to_vec()
            != hex::decode(&intent.client_refund_pubkey).map_err(|_| {
                CoreError::QuoteRejected("client refund key is not hexadecimal".to_owned())
            })?
        || recovery.contract.address != intent.source_destination
        || derived_refund_destination(&session.material) != intent.refund_destination
    {
        return Err(CoreError::Storage(
            "encrypted recovery material does not match the signed quote".to_owned(),
        ));
    }

    Ok(())
}

fn swap_terms(
    intent: &Intent,
    selected: &SelectedQuoteOutput,
    args: &SwapArgs,
    fee_asset_id: AssetId,
) -> CoreResult<SwapTerms> {
    let fee_limit = intent.fee_limit_lbtc.value()?;

    if args.fee_sats > fee_limit {
        return Err(CoreError::QuoteRejected(
            "chain fee exceeds the signed LBTC fee cap".to_owned(),
        ));
    }

    Ok(SwapTerms {
        chain: Chain::LiquidRegtest,
        asset_id: AssetId::from_hex(&args.asset_hash).map_err(chain_error)?,
        fee_asset_id,
        amount_sats: selected.signed_quote.quote.amount_out.value()?,
        asset_amount_sats: selected.signed_quote.quote.amount_in.value()?,
        fee_sats: args.fee_sats,
        refund_lock_height: intent.deadline_context.source_refund_height,
        quote_id: None,
        lightning_cltv_limit: args.lightning_cltv_limit,
        hold_cltv_expiry: args.hold_cltv,
        fee_limit_sats: args.fee_limit_sats,
        payment_timeout: Duration::from_secs(15),
        claim_margin_seconds: Some(args.margin_seconds),
    })
}

fn ensure_swap_record(
    store: &mut SqliteStore,
    intent: &Intent,
    quote: &crate::core::rfq::SignedQuote,
    resume: bool,
) -> CoreResult<()> {
    let existing = store.get_swap(&intent.intent_id)?;

    if let Some(record) = existing {
        if record.intent != *intent || record.quote != *quote {
            return Err(CoreError::Storage(
                "persisted swap binding differs from the requested intent or quote".to_owned(),
            ));
        }

        if !resume && record.state != SwapState::Created {
            return Err(CoreError::InvalidState(
                "new forward requires an unused swap session".to_owned(),
            ));
        }

        return Ok(());
    }

    if resume {
        return Err(CoreError::Storage(
            "resume requires an existing persisted swap".to_owned(),
        ));
    }

    store.insert_swap(&SwapRecord {
        swap_id: intent.intent_id.clone(),
        intent: intent.clone(),
        quote: quote.clone(),
        state: SwapState::Created,
        funding_txid: None,
        claim_txid: None,
        payment_hash: None,
        payment_evidence_json: None,
    })
}

fn prepare_solver_reservation(
    database: &Path,
    intent: &Intent,
    selected: &SelectedQuoteOutput,
    resume: bool,
    existing_state: Option<SwapState>,
) -> CoreResult<()> {
    let mut store = SqliteStore::open(database)?;
    let reservation = store
        .get_reservation(&selected.reservation_id)?
        .ok_or_else(|| CoreError::Storage("selected solver reservation is missing".to_owned()))?;
    let ledger_asset = store
        .get_ledger(&reservation.solver_id)?
        .map(|ledger| ledger.0)
        .ok_or_else(|| CoreError::Storage("selected solver ledger is missing".to_owned()))?;
    let expected_amount = selected.signed_quote.quote.amount_out.to_string();
    let expected_hash = intent.hash_commitment.as_hex();

    if reservation.solver_id != selected.solver_id
        || ledger_asset != selected.signed_quote.quote.destination.asset_id
        || reservation.quote_id != selected.quote_id
        || reservation.amount.to_string() != expected_amount
        || reservation.hash_commitment != expected_hash
        || reservation
            .swap_id
            .as_deref()
            .is_some_and(|value| value != intent.intent_id)
    {
        return Err(CoreError::Storage(
            "solver reservation does not match the selected signed quote".to_owned(),
        ));
    }

    if reservation.state == crate::core::storage::ReservationState::Consumed {
        if !resume
            || !existing_state.is_some_and(|state| {
                matches!(
                    state,
                    SwapState::OutputSettled | SwapState::ClaimPending | SwapState::Settled
                )
            })
        {
            return Err(CoreError::Storage(
                "consumed solver reservation cannot start a new swap".to_owned(),
            ));
        }

        return Ok(());
    }

    if reservation.state != crate::core::storage::ReservationState::Held {
        return Err(CoreError::Storage(
            "selected solver reservation is not held".to_owned(),
        ));
    }

    store.attach_reservation(
        &selected.reservation_id,
        &intent.intent_id,
        now_unix_seconds()?,
    )?;

    Ok(())
}

fn reservation_is_consumed(database: &Path, reservation_id: &str) -> CoreResult<bool> {
    let store = SqliteStore::open(database)?;
    let reservation = store
        .get_reservation(reservation_id)?
        .ok_or_else(|| CoreError::Storage("selected solver reservation is missing".to_owned()))?;

    Ok(reservation.state == crate::core::storage::ReservationState::Consumed)
}

struct CoreSwapObserver {
    store: Mutex<SqliteStore>,
    swap_id: String,
    stop_after_payment: bool,
    solver_database: PathBuf,
    reservation_id: String,
    preimage: [u8; 32],
}

impl CoreSwapObserver {
    fn new(
        store: SqliteStore,
        swap_id: String,
        stop_after_payment: bool,
        solver_database: PathBuf,
        reservation_id: String,
        preimage: [u8; 32],
    ) -> Self {
        Self {
            store: Mutex::new(store),
            swap_id,
            stop_after_payment,
            solver_database,
            reservation_id,
            preimage,
        }
    }

    fn record_evidence(
        &self,
        funding_txid: &str,
        claim_txid: &str,
        payment_hash: &str,
        evidence_json: &str,
    ) -> CoreResult<()> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| CoreError::Storage("swap observer mutex is poisoned".to_owned()))?;
        store
            .record_swap_evidence(
                &self.swap_id,
                funding_txid,
                claim_txid,
                payment_hash,
                evidence_json,
            )
            .map(|_| ())
    }

    fn consume_solver_reservation(&self) -> Result<(), SwapError> {
        let mut solver_store = SqliteStore::open(&self.solver_database)
            .map_err(|error| SwapError::Durability(error.to_string()))?;
        solver_store
            .consume_reservation_verified(&self.reservation_id, &self.preimage)
            .map_err(|error| SwapError::Durability(error.to_string()))?;
        let reservation = solver_store
            .get_reservation(&self.reservation_id)
            .map_err(|error| SwapError::Durability(error.to_string()))?
            .ok_or_else(|| SwapError::Durability("solver reservation disappeared".to_owned()))?;

        if reservation.state != crate::core::storage::ReservationState::Consumed {
            return Err(SwapError::Durability(
                "solver reservation did not reach CONSUMED".to_owned(),
            ));
        }

        Ok(())
    }
}

impl SwapObserver for CoreSwapObserver {
    fn on_phase(&self, phase: SwapPhase) -> Result<(), SwapError> {
        let Some((expected, next)) = phase_transition(phase) else {
            return Ok(());
        };
        let mut store = self
            .store
            .lock()
            .map_err(|_| SwapError::Durability("swap observer mutex is poisoned".to_owned()))?;
        let record = store
            .get_swap(&self.swap_id)
            .map_err(|error| SwapError::Durability(error.to_string()))?
            .ok_or_else(|| SwapError::Durability("swap observer record is missing".to_owned()))?;

        if phase == SwapPhase::OutputSettled
            && (record.state == next || state_rank(record.state) > state_rank(next))
        {
            self.consume_solver_reservation()?;

            return Ok(());
        }

        if record.state == next || state_rank(record.state) > state_rank(next) {
            return Ok(());
        }

        let expected_state =
            if phase == SwapPhase::OutputSettled && record.state == SwapState::OutputUnknown {
                SwapState::OutputUnknown
            } else {
                expected
            };

        if record.state != expected_state {
            return Err(SwapError::RecoveryMismatch(format!(
                "phase {phase:?} expected {expected:?}, found {:?}",
                record.state
            )));
        }

        store
            .transition_swap(&self.swap_id, expected_state, next, None, None)
            .map_err(|error| SwapError::Durability(error.to_string()))?;

        if phase == SwapPhase::OutputSettled {
            self.consume_solver_reservation()?;

            if self.stop_after_payment {
                return Err(SwapError::Unknown(
                    "stop-after-payment hook left the claim for explicit resume".to_owned(),
                ));
            }
        }

        Ok(())
    }
}

fn phase_transition(phase: SwapPhase) -> Option<(SwapState, SwapState)> {
    match phase {
        SwapPhase::RecoveryPrepared => None,
        SwapPhase::InputFunding => Some((SwapState::Created, SwapState::InputFunding)),
        SwapPhase::InputLocked => Some((SwapState::InputFunding, SwapState::InputLocked)),
        SwapPhase::OutputPending => Some((SwapState::InputLocked, SwapState::OutputPending)),
        SwapPhase::OutputUnknown => Some((SwapState::OutputPending, SwapState::OutputUnknown)),
        SwapPhase::OutputSettled => Some((SwapState::OutputPending, SwapState::OutputSettled)),
        SwapPhase::ClaimPending => Some((SwapState::OutputSettled, SwapState::ClaimPending)),
        SwapPhase::Settled => Some((SwapState::ClaimPending, SwapState::Settled)),
        SwapPhase::Refunding => Some((SwapState::InputLocked, SwapState::Refunding)),
        SwapPhase::Refunded => Some((SwapState::Refunding, SwapState::Refunded)),
    }
}

fn state_rank(state: SwapState) -> u8 {
    match state {
        SwapState::Created => 0,
        SwapState::InputFunding => 1,
        SwapState::InputLocked => 2,
        SwapState::OutputPending => 3,
        SwapState::OutputUnknown => 4,
        SwapState::OutputSettled => 5,
        SwapState::ClaimPending => 6,
        SwapState::Settled => 7,
        SwapState::Refunding | SwapState::Refunded | SwapState::Failed => 8,
    }
}

struct BuildIntentRequest<'a> {
    args: &'a PrepareArgs,
    material: &'a SwapKeyMaterial,
    amount_in: u64,
    amount_out: u64,
    fee_limit_lbtc: crate::core::amount::CanonicalAmount,
    deadline_context: DeadlineContext,
    payment_request: String,
    now: u64,
    source_destination: &'a str,
    refund_destination: &'a str,
}

fn build_intent(request: BuildIntentRequest<'_>) -> CoreResult<Intent> {
    let nonce = random_hex(16);

    Ok(Intent {
        version: 1,
        intent_id: request.args.session.clone(),
        source: crate::core::endpoint::EndpointId::new(
            "TEST-DEPIX",
            Network::LiquidRegtest,
            Some(request.args.asset_hash.clone()),
        )?,
        destination: crate::core::endpoint::EndpointId::new(
            "BTC",
            Network::LightningRegtest,
            None,
        )?,
        amount_in: crate::core::amount::CanonicalAmount::parse(&request.amount_in.to_string())?,
        min_amount_out: crate::core::amount::CanonicalAmount::parse(
            &request.amount_out.to_string(),
        )?,
        fee_limit_lbtc: request.fee_limit_lbtc,
        hash_commitment: HashCommitment::parse(&hex::encode(request.material.hash_commitment))?,
        client_claim_pubkey: hex::encode(request.material.claim_public_key.serialize()),
        client_refund_pubkey: hex::encode(request.material.refund_public_key.serialize()),
        source_destination: request.source_destination.to_owned(),
        destination_destination: request.payment_request,
        refund_destination: request.refund_destination.to_owned(),
        deadline_context: request.deadline_context,
        created_at: request.now,
        expires_at: request.now.saturating_add(300),
        nonce,
    })
}

fn derived_refund_destination(material: &SwapKeyMaterial) -> String {
    let public_key = bitcoin::PublicKey::new(material.destination_public_key);

    elements::Address::p2wpkh(
        &public_key,
        None,
        &elements::address::AddressParams::ELEMENTS,
    )
    .to_string()
}

fn lnd_client(
    url: &str,
    certificate_path: &Path,
    macaroon_path: &Path,
) -> CoreResult<LndRestClient> {
    let certificate = fs::read(certificate_path).map_err(storage_error)?;
    let macaroon_bytes = fs::read(macaroon_path).map_err(storage_error)?;
    let macaroon_hex = decode_macaroon(&macaroon_bytes)?;

    LndRestClient::new(LndRestConfig {
        base_url: url.to_owned(),
        tls_certificate_pem: certificate,
        macaroon_hex,
        request_timeout: Duration::from_secs(30),
        network: LightningNetwork::Regtest,
    })
    .map_err(|error| CoreError::Adapter(error.to_string()))
}

fn decode_macaroon(bytes: &[u8]) -> CoreResult<String> {
    let text = std::str::from_utf8(bytes).ok().map(str::trim);

    if let Some(text) = text
        && hex::decode(text).is_ok()
    {
        return Ok(text.to_owned());
    }

    Ok(hex::encode(bytes))
}

fn rpc_client(endpoint: &str, user: &str, password: &str) -> CoreResult<RpcClient> {
    RpcClient::new(endpoint, user, password, Duration::from_secs(15))
        .map_err(|error| CoreError::Transport(error.to_string()))
}

fn elements_height(rpc: &RpcClient) -> CoreResult<u64> {
    let info: serde_json::Value = rpc
        .call("getblockchaininfo", &[])
        .map_err(|error| CoreError::Transport(error.to_string()))?;
    let chain = info.get("chain").and_then(serde_json::Value::as_str);

    if !matches!(chain, Some("elementsregtest" | "liquidregtest")) {
        return Err(CoreError::UnsupportedNetwork(
            chain.unwrap_or("unknown").to_owned(),
        ));
    }

    info.get("blocks")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| CoreError::Adapter("Elements height is missing".to_owned()))
}

fn session_paths(recovery_key: &Path, session: &str) -> SwapPaths {
    let directory = recovery_key
        .parent()
        .unwrap_or_else(|| Path::new(DEFAULT_SESSION_DIR));

    SwapPaths::under(directory, session)
}

fn load_or_create_key(path: &Path) -> CoreResult<[u8; 32]> {
    if path.exists() {
        return read_key(path);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(storage_error)?;
    }

    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path).map_err(storage_error)?;
    file.write_all(hex::encode(bytes).as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(storage_error)?;

    Ok(bytes)
}

fn read_key(path: &Path) -> CoreResult<[u8; 32]> {
    let text = fs::read_to_string(path).map_err(storage_error)?;
    let bytes = hex::decode(text.trim())
        .map_err(|_| CoreError::Storage("recovery key is not hexadecimal".to_owned()))?;

    bytes
        .try_into()
        .map_err(|_| CoreError::Storage("recovery key must contain 32 bytes".to_owned()))
}

fn validate_session_id(session: &str) -> CoreResult<()> {
    if session.is_empty()
        || session.len() > 80
        || !session
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(CoreError::Storage(
            "session must be 1-80 ASCII letters, digits, '-' or '_'".to_owned(),
        ));
    }

    Ok(())
}

fn random_hex(length: usize) -> String {
    let mut bytes = vec![0_u8; length];
    rand::rngs::OsRng.fill_bytes(&mut bytes);

    hex::encode(bytes)
}

fn parse_amount(value: &str) -> CoreResult<u64> {
    crate::core::amount::CanonicalAmount::parse(value)?.value()
}

fn storage_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::Storage(error.to_string())
}

fn chain_error(error: impl std::fmt::Display) -> CoreError {
    CoreError::Adapter(error.to_string())
}

fn swap_error(error: SwapError) -> CoreError {
    CoreError::Adapter(error.to_string())
}

#[derive(Clone, Debug, Serialize)]
struct PreparedOutput {
    session: String,
    intent_id: String,
    hash_commitment: String,
    payment_request: String,
    recovery_path: String,
    intent_path: String,
}

#[derive(Clone, Debug, Serialize)]
struct LiveOutput {
    session_id: String,
    state: String,
    funding_txid: String,
    claim_txid: String,
    accepted_expiry_height: Option<u64>,
    preimage_verified: bool,
    solver_reservation_consumed: bool,
    replay: bool,
}

#[derive(Clone, Debug, Serialize)]
struct PublicEvidence {
    funding_txid: String,
    claim_txid: String,
    claim_confirmations: u32,
    invoice_settled: bool,
    payer_succeeded: bool,
    preimage_verified: bool,
    accepted_expiry_height: Option<u64>,
}

impl From<&crate::swaps::CombinedSwapEvidence> for PublicEvidence {
    fn from(value: &crate::swaps::CombinedSwapEvidence) -> Self {
        Self {
            funding_txid: value.funding_txid.clone(),
            claim_txid: value.claim_txid.clone(),
            claim_confirmations: value.claim_confirmations,
            invoice_settled: value.invoice_settled,
            payer_succeeded: value.payer_succeeded,
            preimage_verified: value.preimage_verified,
            accepted_expiry_height: value.accepted_expiry_height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::amount::CanonicalAmount;
    use crate::core::endpoint::EndpointId;
    use crate::core::rfq::{Quote, SignedQuote};
    use crate::core::storage::{Reservation, ReservationState};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    #[test]
    fn observer_reconciles_held_reservation_after_output_settled_state() {
        let directory = TempDir::new().expect("temp directory");
        let client_path = directory.path().join("client.sqlite");
        let solver_path = directory.path().join("solver.sqlite");
        let preimage = [9_u8; 32];
        let hash = HashCommitment::parse(&hex::encode(Sha256::digest(preimage))).expect("hash");
        let now = 1_700_000_000;
        let intent = Intent {
            version: 1,
            intent_id: "observer-replay".to_owned(),
            source: EndpointId::new("TEST-DEPIX", Network::LiquidRegtest, Some("aa".repeat(32)))
                .expect("source"),
            destination: EndpointId::new("BTC", Network::LightningRegtest, None)
                .expect("destination"),
            amount_in: CanonicalAmount::parse("1000").expect("input"),
            min_amount_out: CanonicalAmount::parse("1000").expect("minimum"),
            fee_limit_lbtc: CanonicalAmount::parse("50").expect("fee"),
            hash_commitment: hash.clone(),
            client_claim_pubkey: "claim".to_owned(),
            client_refund_pubkey: "refund".to_owned(),
            source_destination: "contract".to_owned(),
            destination_destination: "invoice".to_owned(),
            refund_destination: "refund-destination".to_owned(),
            deadline_context: DeadlineContext {
                source_height: 100,
                destination_height: 80,
                source_refund_height: 1_200,
                destination_refund_height: 90,
                lightning_expiry_height: Some(100),
                source_refund_at: now + 20_000,
                destination_refund_at: now + 10_000,
                lightning_expiry_at: Some(now + 10_000),
                margin_seconds: 7_200,
            },
            created_at: now,
            expires_at: now + 300,
            nonce: "nonce".to_owned(),
        };
        let quote = Quote {
            version: 1,
            quote_id: "quote-observer".to_owned(),
            intent_id: intent.intent_id.clone(),
            source: intent.source.clone(),
            destination: intent.destination.clone(),
            amount_in: intent.amount_in.clone(),
            min_amount_out: intent.min_amount_out.clone(),
            amount_out: intent.amount_in.clone(),
            fee_lbtc: CanonicalAmount::parse("1").expect("fee"),
            fee_limit_lbtc: intent.fee_limit_lbtc.clone(),
            hash_commitment: hash,
            client_claim_pubkey: intent.client_claim_pubkey.clone(),
            client_refund_pubkey: intent.client_refund_pubkey.clone(),
            source_destination: intent.source_destination.clone(),
            destination_destination: intent.destination_destination.clone(),
            refund_destination: intent.refund_destination.clone(),
            source_refund_height: intent.deadline_context.source_refund_height,
            destination_refund_height: intent.deadline_context.destination_refund_height,
            lightning_expiry_height: intent.deadline_context.lightning_expiry_height,
            deadline_context: intent.deadline_context.clone(),
            expires_at: intent.expires_at,
            nonce: intent.nonce.clone(),
            solver_key_id: "solver-key".to_owned(),
            adapter_id: "lightning-hold".to_owned(),
            payment_request: None,
        };
        let mut client = SqliteStore::open(&client_path).expect("client");
        client
            .insert_swap(&SwapRecord {
                swap_id: intent.intent_id.clone(),
                intent: intent.clone(),
                quote: SignedQuote {
                    quote,
                    signature_hex: "00".repeat(64),
                },
                state: SwapState::OutputSettled,
                funding_txid: None,
                claim_txid: None,
                payment_hash: None,
                payment_evidence_json: None,
            })
            .expect("client record");
        let mut solver = SqliteStore::open(&solver_path).expect("solver");
        solver
            .ensure_solver_ledger(
                "solver-a",
                "BTC",
                &CanonicalAmount::parse("5000").expect("inventory"),
            )
            .expect("ledger");
        solver
            .reserve(
                &Reservation {
                    reservation_id: "reservation-observer".to_owned(),
                    solver_id: "solver-a".to_owned(),
                    quote_id: "quote-observer".to_owned(),
                    swap_id: None,
                    amount: CanonicalAmount::parse("1000").expect("amount"),
                    hash_commitment: hex::encode(Sha256::digest(preimage)),
                    state: ReservationState::Held,
                    expires_at: now + 100,
                },
                "BTC",
            )
            .expect("reserve");
        solver
            .attach_reservation("reservation-observer", &intent.intent_id, now)
            .expect("attach");
        drop(solver);
        let observer = CoreSwapObserver::new(
            client,
            intent.intent_id,
            false,
            solver_path.clone(),
            "reservation-observer".to_owned(),
            preimage,
        );

        observer
            .on_phase(SwapPhase::OutputSettled)
            .expect("reconcile");
        let solver = SqliteStore::open(&solver_path).expect("reopen solver");
        let reservation = solver
            .get_reservation("reservation-observer")
            .expect("reservation")
            .expect("row");
        assert_eq!(reservation.state, ReservationState::Consumed);
    }
}
