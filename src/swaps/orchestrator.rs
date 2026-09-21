//! Durable forward and reverse HTLC swap orchestration.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::chain::{AssetId, Chain, FundingEvidence, HtlcContract};
use crate::lightning::validate_payment_request;
use crate::lightning::{
    CancelHoldRequest, HoldInvoice, HoldInvoiceRequest, InvoiceState, LightningAdapter,
    LightningError, PaymentRequest, PaymentState,
};

use super::adapter::{
    ClaimEvidence, PrepareFundingRequest, PrepareSpendRequest, PreparedTransaction,
    SpendKeyMaterial, SwapChainAdapter,
};
use super::error::SwapError;
use super::outbox::{FundingOutbox, OutboxKind, read_outbox, write_outbox};
use super::recovery::{
    PrepareRecoveryRequest, SwapDirection, SwapKeyMaterial, SwapRecoveryRecord,
    prepare_or_load_recovery,
};
use super::timing::{validate_forward_timing, validate_reverse_timing};

const DEFAULT_LIGHTNING_CLTV: u32 = 80;
const DEFAULT_HOLD_CLTV: u32 = 40;
const DEFAULT_FEE_LIMIT_SATS: u64 = 20;
const DEFAULT_PAYMENT_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_TRACK_TIMEOUT: Duration = Duration::from_secs(15);
const MIN_ACCEPTED_HEADROOM_BLOCKS: u64 = 6;

/// Paths for the encrypted recovery record and transaction outboxes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwapPaths {
    /// Encrypted key material and contract record.
    pub recovery: PathBuf,
    /// Funding transaction outbox.
    pub funding: PathBuf,
    /// Claim transaction outbox.
    pub claim: PathBuf,
    /// Refund transaction outbox.
    pub refund: PathBuf,
}

impl SwapPaths {
    /// Creates the standard outbox names below one private directory.
    pub fn under(directory: impl AsRef<Path>, prefix: &str) -> Self {
        let directory = directory.as_ref();

        Self {
            recovery: directory.join(format!("{prefix}-recovery.json")),
            funding: directory.join(format!("{prefix}-funding.json")),
            claim: directory.join(format!("{prefix}-claim.json")),
            refund: directory.join(format!("{prefix}-refund.json")),
        }
    }
}

/// Quote-bound terms required by both swap directions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwapTerms {
    /// Chain carrying the explicit HTLC asset.
    pub chain: Chain,
    /// Explicit payment asset on that chain.
    pub asset_id: AssetId,
    /// Explicit fee asset on that chain.
    pub fee_asset_id: AssetId,
    /// Lightning amount in satoshis.
    pub amount_sats: u64,
    /// Chain HTLC amount in satoshis.
    pub asset_amount_sats: u64,
    /// Explicit chain miner fee in satoshis.
    pub fee_sats: u64,
    /// Absolute refund height on the chain carrying the asset.
    pub refund_lock_height: u64,
    /// Optional quote identifier bound to recovery.
    pub quote_id: Option<String>,
    /// Lightning routing CLTV safety bound.
    pub lightning_cltv_limit: u32,
    /// Lightning hold invoice CLTV delta.
    pub hold_cltv_expiry: u32,
    /// Maximum Lightning routing fee.
    pub fee_limit_sats: u64,
    /// Router call timeout. A timeout remains recoverable and unknown.
    pub payment_timeout: Duration,
    /// Extra time reserved for claim observation and chain confirmation.
    pub claim_margin_seconds: Option<u64>,
}

/// Values required to construct conservative default swap terms.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwapTermsDefaults {
    /// Chain carrying the explicit HTLC asset.
    pub chain: Chain,
    /// Explicit payment asset on that chain.
    pub asset_id: AssetId,
    /// Explicit fee asset on that chain.
    pub fee_asset_id: AssetId,
    /// Lightning amount in satoshis.
    pub amount_sats: u64,
    /// Chain HTLC amount in satoshis.
    pub asset_amount_sats: u64,
    /// Explicit chain miner fee in satoshis.
    pub fee_sats: u64,
    /// Absolute refund height on the chain carrying the asset.
    pub refund_lock_height: u64,
    /// Optional quote identifier bound to recovery.
    pub quote_id: Option<String>,
}

impl SwapTerms {
    /// Creates conservative default Lightning bounds for the regtest POC.
    pub fn with_defaults(args: SwapTermsDefaults) -> Self {
        Self {
            chain: args.chain,
            asset_id: args.asset_id,
            fee_asset_id: args.fee_asset_id,
            amount_sats: args.amount_sats,
            asset_amount_sats: args.asset_amount_sats,
            fee_sats: args.fee_sats,
            refund_lock_height: args.refund_lock_height,
            quote_id: args.quote_id,
            lightning_cltv_limit: DEFAULT_LIGHTNING_CLTV,
            hold_cltv_expiry: DEFAULT_HOLD_CLTV,
            fee_limit_sats: DEFAULT_FEE_LIMIT_SATS,
            payment_timeout: DEFAULT_PAYMENT_TIMEOUT,
            claim_margin_seconds: None,
        }
    }
}

/// Durable state phases emitted around every external swap effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SwapPhase {
    /// Recovery material and the contract have been persisted.
    RecoveryPrepared,
    /// Signed funding transaction has been persisted before broadcast.
    InputFunding,
    /// Funding transaction was observed with the expected HTLC output.
    InputLocked,
    /// The Lightning payment attempt is about to start.
    OutputPending,
    /// The payment result is not yet known.
    OutputUnknown,
    /// Lightning settlement and the verified payer preimage are complete.
    OutputSettled,
    /// Claim transaction is persisted before broadcast.
    ClaimPending,
    /// Claim witness was confirmed and checked against the commitment.
    Settled,
    /// A cooperative refund is being attempted.
    Refunding,
    /// A cooperative refund was confirmed.
    Refunded,
}

/// Observer used by the core state machine to make phase transitions durable.
pub trait SwapObserver {
    /// Persist one phase before the associated external effect.
    fn on_phase(&self, phase: SwapPhase) -> Result<(), SwapError>;
}

/// Default observer for callers that do not have a core state store.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopSwapObserver;

impl SwapObserver for NoopSwapObserver {
    fn on_phase(&self, _phase: SwapPhase) -> Result<(), SwapError> {
        Ok(())
    }
}

/// A caller-owned recovery key for encrypting one swap session.
#[derive(Clone, Eq, PartialEq)]
pub struct SwapEncryptionKey(pub [u8; 32]);

impl std::fmt::Debug for SwapEncryptionKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SwapEncryptionKey(<redacted>)")
    }
}

/// Inputs for a forward swap where the user funds Liquid and receives Lightning.
pub struct ForwardSwapRequest<'a, C, R, P> {
    /// Chain adapter used for explicit Liquid funding and claim.
    pub chain: &'a C,
    /// Receiver's hold invoice node.
    pub receiver: &'a R,
    /// User's payer node.
    pub payer: &'a P,
    /// Durable recovery and outbox paths.
    pub paths: SwapPaths,
    /// Encryption key kept outside public evidence.
    pub encryption_key: SwapEncryptionKey,
    /// Quote-bound swap terms.
    pub terms: SwapTerms,
    /// Optional deterministic key material supplied by a resumed caller.
    pub supplied_material: Option<SwapKeyMaterial>,
    /// Optional invoice memo retained only by LND.
    pub memo: Option<String>,
    /// Core state observer.
    pub observer: &'a dyn SwapObserver,
}

/// Inputs for a reverse swap where the user pays Lightning and receives Liquid.
pub struct ReverseSwapRequest<'a, C, R, P> {
    /// Chain adapter used for explicit Liquid funding and claim.
    pub chain: &'a C,
    /// Receiver's hold invoice node.
    pub receiver: &'a R,
    /// User's payer node.
    pub payer: &'a P,
    /// Durable recovery and outbox paths.
    pub paths: SwapPaths,
    /// Encryption key kept outside public evidence.
    pub encryption_key: SwapEncryptionKey,
    /// Quote-bound swap terms.
    pub terms: SwapTerms,
    /// Optional deterministic key material supplied by a resumed caller.
    pub supplied_material: Option<SwapKeyMaterial>,
    /// Optional invoice memo retained only by LND.
    pub memo: Option<String>,
    /// Core state observer.
    pub observer: &'a dyn SwapObserver,
}

/// Sanitized evidence returned after both HTLC branches settle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CombinedSwapEvidence {
    /// Direction of the completed swap.
    pub direction: SwapDirection,
    /// Chain funding transaction identifier.
    pub funding_txid: String,
    /// Chain claim transaction identifier.
    pub claim_txid: String,
    /// Confirmations on the chain claim.
    pub claim_confirmations: u32,
    /// Lightning invoice reached SETTLED.
    pub invoice_settled: bool,
    /// Lightning payer reached SUCCEEDED.
    pub payer_succeeded: bool,
    /// The chain witness was checked against the same commitment.
    pub preimage_verified: bool,
    /// Actual accepted Lightning HTLC expiry when available.
    pub accepted_expiry_height: Option<u64>,
}

/// Evidence for a completed cooperative refund after a failed payment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefundSwapEvidence {
    /// Funding transaction that carried the HTLC.
    pub funding_txid: String,
    /// Refund transaction identifier.
    pub refund_txid: String,
    /// Confirmations on the refund.
    pub confirmations: u32,
}

/// Runs the user-funds-Liquid direction with durable replay.
pub fn run_forward_swap<C, R, P>(
    request: ForwardSwapRequest<'_, C, R, P>,
) -> Result<CombinedSwapEvidence, SwapError>
where
    C: SwapChainAdapter,
    R: LightningAdapter,
    P: LightningAdapter,
{
    let (record, _) = load_recovery(
        &request.paths,
        &request.encryption_key,
        SwapDirection::DepixToLightning,
        &request.terms,
        request.supplied_material.clone(),
    )?;
    request.observer.on_phase(SwapPhase::RecoveryPrepared)?;
    let invoice = ensure_invoice(
        request.receiver,
        &record.session.material.hash_commitment,
        &request.terms,
        request.memo.clone(),
    )?;
    validate_open_invoice(&invoice, request.terms.amount_sats)?;

    if let Some(evidence) = replay_completed(
        request.chain,
        request.receiver,
        request.payer,
        &request.paths,
        &record,
        request.observer,
        SwapDirection::DepixToLightning,
    )? {
        return Ok(evidence);
    }

    let mut funding_exposed = read_funding(&request.paths.funding)?.is_some();
    let result = run_forward_after_invoice(&request, &record, &invoice, &mut funding_exposed);

    if result.is_err() && !funding_exposed {
        cancel_if_accepted(
            request.receiver,
            &record.session.material.hash_commitment,
            record.session.amount_sats,
        );
    }

    result
}

/// Runs the user-pays-Lightning direction with durable replay.
pub fn run_reverse_swap<C, R, P>(
    request: ReverseSwapRequest<'_, C, R, P>,
) -> Result<CombinedSwapEvidence, SwapError>
where
    C: SwapChainAdapter,
    R: LightningAdapter,
    P: LightningAdapter,
{
    let (record, _) = load_recovery(
        &request.paths,
        &request.encryption_key,
        SwapDirection::LightningToDepix,
        &request.terms,
        request.supplied_material.clone(),
    )?;
    request.observer.on_phase(SwapPhase::RecoveryPrepared)?;
    let invoice = ensure_invoice(
        request.receiver,
        &record.session.material.hash_commitment,
        &request.terms,
        request.memo.clone(),
    )?;
    validate_open_invoice(&invoice, request.terms.amount_sats)?;

    if let Some(evidence) = replay_completed(
        request.chain,
        request.receiver,
        request.payer,
        &request.paths,
        &record,
        request.observer,
        SwapDirection::LightningToDepix,
    )? {
        return Ok(evidence);
    }

    let mut funding_exposed = read_funding(&request.paths.funding)?.is_some();
    let result = run_reverse_after_invoice(&request, &record, &invoice, &mut funding_exposed);

    if result.is_err() && !funding_exposed {
        cancel_if_accepted(
            request.receiver,
            &record.session.material.hash_commitment,
            record.session.amount_sats,
        );
    }

    result
}

/// Refunds an already funded Liquid HTLC after its absolute lock height.
pub fn refund_funded_swap<C, R, P>(
    chain: &C,
    receiver: &R,
    payer: &P,
    paths: &SwapPaths,
    encryption_key: &SwapEncryptionKey,
    amount_sats: u64,
) -> Result<RefundSwapEvidence, SwapError>
where
    C: SwapChainAdapter,
    R: LightningAdapter,
    P: LightningAdapter,
{
    refund_funded_swap_with_observer(
        chain,
        receiver,
        payer,
        paths,
        encryption_key,
        amount_sats,
        &NoopSwapObserver,
    )
}

/// Refunds a funded HTLC while emitting durable core state phases.
pub fn refund_funded_swap_with_observer<C, R, P>(
    chain: &C,
    receiver: &R,
    payer: &P,
    paths: &SwapPaths,
    encryption_key: &SwapEncryptionKey,
    amount_sats: u64,
    observer: &dyn SwapObserver,
) -> Result<RefundSwapEvidence, SwapError>
where
    C: SwapChainAdapter,
    R: LightningAdapter,
    P: LightningAdapter,
{
    let record = super::recovery::read_recovery(&paths.recovery, &encryption_key.0)?
        .ok_or_else(|| SwapError::Durability("swap recovery record is missing".to_owned()))?;
    if record.session.amount_sats != amount_sats {
        return Err(SwapError::RecoveryMismatch(
            "refund amount differs from the recovery record".to_owned(),
        ));
    }

    let payment = payer
        .track_by_hash(
            record.session.material.hash_commitment,
            DEFAULT_TRACK_TIMEOUT,
        )
        .map_err(|error| SwapError::Lightning(error.to_string()))?;
    if payment.state != PaymentState::Failed {
        return Err(SwapError::Unknown(
            "refund requires a terminal failed Lightning payment".to_owned(),
        ));
    }

    let invoice = receiver
        .lookup_invoice(record.session.material.hash_commitment)
        .map_err(|error| SwapError::Lightning(error.to_string()))?;
    if invoice.state != InvoiceState::Canceled
        || invoice.amount_sats != amount_sats
        || invoice.accepted_amount_sats != Some(amount_sats)
    {
        return Err(SwapError::Unknown(
            "refund requires a canceled hold invoice with the quoted amount".to_owned(),
        ));
    }

    if read_outbox(&paths.claim)?.is_some() {
        return Err(SwapError::RecoveryMismatch(
            "a claim outbox exists; refund is no longer safe".to_owned(),
        ));
    }
    let funding_outbox = read_funding(&paths.funding)?
        .ok_or_else(|| SwapError::Unknown("funding outbox is missing".to_owned()))?;
    let funding = FundingEvidence {
        txid: funding_outbox.txid.clone(),
        vout: funding_outbox.funding_vout.unwrap_or(0),
        amount_sats: record.session.asset_amount_sats,
        asset_id: record.session.asset_id,
        confirmations: 1,
    };
    let height = chain.current_height()?;

    if height < record.contract.refund_lock_height {
        return Err(SwapError::Chain(format!(
            "refund is early at height {height}; requires {}",
            record.contract.refund_lock_height
        )));
    }

    let existing = read_outbox(&paths.refund)?;
    let outbox = match existing {
        Some(outbox) => validate_spend_outbox(outbox, OutboxKind::Refund, &funding)?,
        None => {
            let prepared = chain.prepare_spend(PrepareSpendRequest {
                contract: record.contract.clone(),
                funding: funding.clone(),
                fee_sats: record.session.fee_sats,
                fee_asset_id: record.session.fee_asset_id,
                kind: super::super::chain::SpendKind::Refund,
                keys: SpendKeyMaterial {
                    branch_private_key: record.session.material.refund_private_key,
                    destination_public_key: record.session.material.destination_public_key,
                    preimage: None,
                },
            })?;
            persist_spend(&paths.refund, prepared)?
        }
    };
    observer.on_phase(SwapPhase::Refunding)?;
    let _ = chain.broadcast(&outbox)?;
    let evidence = match chain.observe_refund(&outbox) {
        Ok(evidence) => evidence,
        Err(_) => {
            chain.confirm_spend(&outbox)?;
            chain.observe_refund(&outbox)?
        }
    };
    observer.on_phase(SwapPhase::Refunded)?;

    Ok(RefundSwapEvidence {
        funding_txid: evidence.funding_txid,
        refund_txid: evidence.txid,
        confirmations: evidence.confirmations,
    })
}

fn run_forward_after_invoice<C, R, P>(
    request: &ForwardSwapRequest<'_, C, R, P>,
    record: &SwapRecoveryRecord,
    invoice: &HoldInvoice,
    funding_exposed: &mut bool,
) -> Result<CombinedSwapEvidence, SwapError>
where
    C: SwapChainAdapter,
    R: LightningAdapter,
    P: LightningAdapter,
{
    let height = request.chain.current_height()?;
    let settled_recovery =
        invoice.state == InvoiceState::Settled && read_funding(&request.paths.funding)?.is_some();

    if !settled_recovery {
        let refund_delta = record
            .contract
            .refund_lock_height
            .checked_sub(height)
            .ok_or_else(|| SwapError::Timing("Liquid refund lock is already past".to_owned()))?;

        validate_forward_timing(
            u64::from(request.terms.lightning_cltv_limit),
            refund_delta,
            request.terms.claim_margin_seconds,
        )?;
    }
    let funding = prepare_or_replay_funding(
        request.chain,
        &request.paths.funding,
        &record.contract,
        &request.terms,
        request.observer,
        funding_exposed,
    )?;
    request.observer.on_phase(SwapPhase::OutputPending)?;
    pay_if_open(
        request.payer,
        invoice,
        &record.session.material.hash_commitment,
        &request.terms,
    )?;
    let accepted = request
        .receiver
        .observe_accepted(
            record.session.material.hash_commitment,
            request.terms.amount_sats,
        )
        .map_err(|error| SwapError::Lightning(error.to_string()))?;
    let tip = request
        .receiver
        .current_tip()
        .map_err(|error| SwapError::Lightning(error.to_string()))?;
    validate_accepted(&accepted, request.terms.amount_sats, tip.block_height)?;
    settle_invoice(
        request.receiver,
        &accepted,
        &record.session.material.preimage,
    )?;
    let payment = track_success(
        request.payer,
        record.session.material.hash_commitment,
        request.terms.payment_timeout,
    )?;
    verify_payment_preimage(&payment, &record.session.material.preimage)?;
    request.observer.on_phase(SwapPhase::OutputSettled)?;
    let claim = claim_or_replay(
        request.chain,
        &request.paths.claim,
        record,
        &funding,
        request.observer,
        funding_exposed,
    )?;

    request.observer.on_phase(SwapPhase::Settled)?;
    build_evidence(
        SwapDirection::DepixToLightning,
        &funding,
        &claim,
        accepted.accepted_expiry_height,
    )
}

fn run_reverse_after_invoice<C, R, P>(
    request: &ReverseSwapRequest<'_, C, R, P>,
    record: &SwapRecoveryRecord,
    invoice: &HoldInvoice,
    funding_exposed: &mut bool,
) -> Result<CombinedSwapEvidence, SwapError>
where
    C: SwapChainAdapter,
    R: LightningAdapter,
    P: LightningAdapter,
{
    request.observer.on_phase(SwapPhase::OutputPending)?;
    pay_if_open(
        request.payer,
        invoice,
        &record.session.material.hash_commitment,
        &request.terms,
    )?;
    let accepted = request
        .receiver
        .observe_accepted(
            record.session.material.hash_commitment,
            request.terms.amount_sats,
        )
        .map_err(|error| SwapError::Lightning(error.to_string()))?;
    let tip = request
        .receiver
        .current_tip()
        .map_err(|error| SwapError::Lightning(error.to_string()))?;
    validate_accepted(&accepted, request.terms.amount_sats, tip.block_height)?;
    let liquid_height = request.chain.current_height()?;
    let refund_delta = record
        .contract
        .refund_lock_height
        .checked_sub(liquid_height)
        .ok_or_else(|| SwapError::Timing("Liquid refund lock is already past".to_owned()))?;
    let expiry = accepted
        .accepted_expiry_height
        .ok_or_else(|| SwapError::Timing("accepted Lightning HTLC expiry is missing".to_owned()))?;
    validate_reverse_timing(
        tip.block_height,
        expiry,
        refund_delta,
        request.terms.claim_margin_seconds,
    )?;
    let funding = prepare_or_replay_funding(
        request.chain,
        &request.paths.funding,
        &record.contract,
        &request.terms,
        request.observer,
        funding_exposed,
    )?;
    let claim = claim_or_replay(
        request.chain,
        &request.paths.claim,
        record,
        &funding,
        request.observer,
        funding_exposed,
    )?;
    settle_invoice(
        request.receiver,
        &accepted,
        &record.session.material.preimage,
    )?;
    let payment = track_success(
        request.payer,
        record.session.material.hash_commitment,
        request.terms.payment_timeout,
    )?;

    verify_payment_preimage(&payment, &record.session.material.preimage)?;
    request.observer.on_phase(SwapPhase::OutputSettled)?;
    request.observer.on_phase(SwapPhase::Settled)?;
    build_evidence(
        SwapDirection::LightningToDepix,
        &funding,
        &claim,
        accepted.accepted_expiry_height,
    )
}

fn load_recovery(
    paths: &SwapPaths,
    encryption_key: &SwapEncryptionKey,
    direction: SwapDirection,
    terms: &SwapTerms,
    supplied_material: Option<SwapKeyMaterial>,
) -> Result<(SwapRecoveryRecord, bool), SwapError> {
    prepare_or_load_recovery(PrepareRecoveryRequest {
        path: &paths.recovery,
        encryption_key: &encryption_key.0,
        direction,
        chain: terms.chain,
        asset_id: terms.asset_id,
        fee_asset_id: terms.fee_asset_id,
        amount_sats: terms.amount_sats,
        asset_amount_sats: terms.asset_amount_sats,
        fee_sats: terms.fee_sats,
        refund_lock_height: terms.refund_lock_height,
        quote_id: terms.quote_id.clone(),
        supplied_material,
    })
}

fn ensure_invoice<L: LightningAdapter>(
    receiver: &L,
    hash: &[u8; 32],
    terms: &SwapTerms,
    memo: Option<String>,
) -> Result<HoldInvoice, SwapError> {
    match receiver.lookup_invoice(*hash) {
        Ok(invoice) => {
            validate_invoice_amount(&invoice, terms.amount_sats)?;
            Ok(invoice)
        }
        Err(LightningError::Http { status: 404, .. }) => receiver
            .create_hold_invoice(HoldInvoiceRequest {
                payment_hash: *hash,
                amount_sats: terms.amount_sats,
                cltv_expiry: terms.hold_cltv_expiry,
                memo,
            })
            .map_err(|error| SwapError::Lightning(error.to_string())),
        Err(error) => Err(SwapError::Lightning(error.to_string())),
    }
}

fn validate_open_invoice(invoice: &HoldInvoice, amount_sats: u64) -> Result<(), SwapError> {
    if invoice.state != InvoiceState::Open {
        return Ok(());
    }

    let payment_request = invoice.payment_request.as_deref().ok_or_else(|| {
        SwapError::Lightning("open hold invoice lacks a payment request".to_owned())
    })?;
    validate_payment_request(payment_request, invoice.payment_hash, amount_sats)
        .map(|_| ())
        .map_err(|error| SwapError::Lightning(error.to_string()))
}

fn validate_invoice_amount(invoice: &HoldInvoice, amount_sats: u64) -> Result<(), SwapError> {
    if invoice.amount_sats != amount_sats {
        return Err(SwapError::RecoveryMismatch(
            "existing hold invoice amount differs from the quote".to_owned(),
        ));
    }

    if invoice.state == InvoiceState::Canceled {
        return Err(SwapError::Lightning(
            "existing hold invoice was canceled".to_owned(),
        ));
    }

    if invoice.state == InvoiceState::Open && invoice.payment_request.is_none() {
        return Err(SwapError::Lightning(
            "open hold invoice did not return a payment request".to_owned(),
        ));
    }

    Ok(())
}

fn pay_if_open<L: LightningAdapter>(
    payer: &L,
    invoice: &HoldInvoice,
    hash: &[u8; 32],
    terms: &SwapTerms,
) -> Result<(), SwapError> {
    if invoice.state != InvoiceState::Open {
        return Ok(());
    }

    let payment_request = invoice
        .payment_request
        .as_ref()
        .ok_or_else(|| SwapError::Lightning("payment request is missing".to_owned()))?;
    let observation = payer
        .pay(PaymentRequest {
            payment_request: payment_request.clone(),
            payment_hash: *hash,
            expected_amount_sats: terms.amount_sats,
            fee_limit_sats: terms.fee_limit_sats,
            cltv_limit: terms.lightning_cltv_limit,
            timeout: terms.payment_timeout,
        })
        .map_err(|error| SwapError::Lightning(error.to_string()))?;

    if observation.state == PaymentState::Failed {
        return Err(SwapError::Lightning(
            "Lightning payment failed before the hold was accepted".to_owned(),
        ));
    }

    Ok(())
}

fn validate_accepted(
    invoice: &HoldInvoice,
    amount_sats: u64,
    current_height: u64,
) -> Result<(), SwapError> {
    if invoice.state != InvoiceState::Accepted && invoice.state != InvoiceState::Settled {
        return Err(SwapError::Unknown(format!(
            "Lightning invoice is {:?}, not accepted",
            invoice.state
        )));
    }

    if invoice.accepted_amount_sats != Some(amount_sats) {
        return Err(SwapError::Lightning(
            "accepted Lightning amount does not match the quote".to_owned(),
        ));
    }

    if invoice.state == InvoiceState::Settled {
        return Ok(());
    }

    let expiry = invoice.accepted_expiry_height.ok_or_else(|| {
        SwapError::Unknown("accepted Lightning HTLC expiry is unavailable".to_owned())
    })?;
    let required_height = current_height
        .checked_add(MIN_ACCEPTED_HEADROOM_BLOCKS)
        .ok_or_else(|| SwapError::Timing("Lightning expiry height overflows".to_owned()))?;

    if expiry <= required_height {
        return Err(SwapError::Timing(
            "accepted Lightning HTLC has insufficient headroom".to_owned(),
        ));
    }

    Ok(())
}

fn settle_invoice<L: LightningAdapter>(
    receiver: &L,
    invoice: &HoldInvoice,
    preimage: &[u8; 32],
) -> Result<(), SwapError> {
    if invoice.state == InvoiceState::Settled {
        return Ok(());
    }

    receiver
        .settle_hold(invoice.payment_hash, *preimage)
        .map_err(|error| SwapError::Lightning(error.to_string()))?;

    Ok(())
}

fn track_success<L: LightningAdapter>(
    payer: &L,
    hash: [u8; 32],
    timeout: Duration,
) -> Result<crate::lightning::PaymentObservation, SwapError> {
    let observation = payer
        .track_by_hash(hash, timeout.max(DEFAULT_TRACK_TIMEOUT))
        .map_err(|error| SwapError::Lightning(error.to_string()))?;

    if observation.state != PaymentState::Succeeded {
        return Err(SwapError::Unknown(format!(
            "Lightning payment ended in {:?}",
            observation.state
        )));
    }

    Ok(observation)
}

fn verify_payment_preimage(
    observation: &crate::lightning::PaymentObservation,
    expected: &[u8; 32],
) -> Result<(), SwapError> {
    if observation.payment_preimage != Some(*expected) {
        return Err(SwapError::Lightning(
            "Lightning success did not return the committed preimage".to_owned(),
        ));
    }

    Ok(())
}

fn prepare_or_replay_funding<C: SwapChainAdapter>(
    chain: &C,
    path: &Path,
    contract: &HtlcContract,
    terms: &SwapTerms,
    observer: &dyn SwapObserver,
    funding_exposed: &mut bool,
) -> Result<FundingEvidence, SwapError> {
    let outbox = match read_funding(path)? {
        Some(outbox) => validate_funding_outbox(outbox)?,
        None => {
            let prepared = chain.prepare_funding(PrepareFundingRequest {
                contract: contract.clone(),
                amount_sats: terms.asset_amount_sats,
                fee_sats: terms.fee_sats,
                fee_asset_id: terms.fee_asset_id,
            })?;
            persist_funding(path, prepared)?
        }
    };
    *funding_exposed = true;
    observer.on_phase(SwapPhase::InputFunding)?;
    let _ = chain.broadcast(&outbox)?;
    let funding = chain.observe_funding(
        contract,
        &outbox.txid,
        outbox.funding_vout.unwrap_or(0),
        terms.asset_amount_sats,
    );
    let funding = match funding {
        Ok(funding) => funding,
        Err(_) => {
            chain.confirm_funding(contract, &outbox)?;
            chain.observe_funding(
                contract,
                &outbox.txid,
                outbox.funding_vout.unwrap_or(0),
                terms.asset_amount_sats,
            )?
        }
    };
    observer.on_phase(SwapPhase::InputLocked)?;

    Ok(funding)
}

fn claim_or_replay<C: SwapChainAdapter>(
    chain: &C,
    path: &Path,
    record: &SwapRecoveryRecord,
    funding: &FundingEvidence,
    observer: &dyn SwapObserver,
    funding_exposed: &mut bool,
) -> Result<ClaimEvidence, SwapError> {
    let outbox = match read_outbox(path)? {
        Some(outbox) => validate_spend_outbox(outbox, OutboxKind::Claim, funding)?,
        None => {
            let prepared = chain.prepare_spend(PrepareSpendRequest {
                contract: record.contract.clone(),
                funding: funding.clone(),
                fee_sats: record.session.fee_sats,
                fee_asset_id: record.session.fee_asset_id,
                kind: super::super::chain::SpendKind::Claim,
                keys: SpendKeyMaterial {
                    branch_private_key: record.session.material.claim_private_key,
                    destination_public_key: record.session.material.destination_public_key,
                    preimage: Some(record.session.material.preimage),
                },
            })?;
            persist_spend(path, prepared)?
        }
    };
    *funding_exposed = true;
    observer.on_phase(SwapPhase::ClaimPending)?;
    let _ = chain.broadcast(&outbox)?;
    let claim = chain.observe_claim(&record.contract, &outbox, record.session.material.preimage);

    match claim {
        Ok(claim) => Ok(claim),
        Err(_) => {
            chain.confirm_spend(&outbox)?;
            chain.observe_claim(&record.contract, &outbox, record.session.material.preimage)
        }
    }
}

fn replay_completed<C, R, P>(
    chain: &C,
    receiver: &R,
    payer: &P,
    paths: &SwapPaths,
    record: &SwapRecoveryRecord,
    observer: &dyn SwapObserver,
    direction: SwapDirection,
) -> Result<Option<CombinedSwapEvidence>, SwapError>
where
    C: SwapChainAdapter,
    R: LightningAdapter,
    P: LightningAdapter,
{
    let Some(outbox) = read_outbox(&paths.claim)? else {
        return Ok(None);
    };
    let outbox = validate_claim_outbox(outbox)?;
    let _ = chain.broadcast(&outbox)?;
    let claim =
        match chain.observe_claim(&record.contract, &outbox, record.session.material.preimage) {
            Ok(claim) => claim,
            Err(_) => {
                chain.confirm_spend(&outbox)?;
                chain.observe_claim(&record.contract, &outbox, record.session.material.preimage)?
            }
        };
    let current_invoice = receiver
        .observe_accepted(
            record.session.material.hash_commitment,
            record.session.amount_sats,
        )
        .map_err(|error| SwapError::Lightning(error.to_string()))?;
    let tip = receiver
        .current_tip()
        .map_err(|error| SwapError::Lightning(error.to_string()))?;
    validate_accepted(
        &current_invoice,
        record.session.amount_sats,
        tip.block_height,
    )?;
    settle_invoice(
        receiver,
        &current_invoice,
        &record.session.material.preimage,
    )?;
    let payment = track_success(
        payer,
        record.session.material.hash_commitment,
        DEFAULT_TRACK_TIMEOUT,
    )?;
    verify_payment_preimage(&payment, &record.session.material.preimage)?;
    observer.on_phase(SwapPhase::OutputSettled)?;
    observer.on_phase(SwapPhase::Settled)?;
    let funding_txid = outbox
        .funding_txid
        .clone()
        .ok_or_else(|| SwapError::Durability("claim outbox lacks funding txid".to_owned()))?;

    Ok(Some(CombinedSwapEvidence {
        direction,
        funding_txid,
        claim_txid: claim.txid,
        claim_confirmations: claim.confirmations,
        invoice_settled: true,
        payer_succeeded: true,
        preimage_verified: claim.preimage_verified,
        accepted_expiry_height: current_invoice.accepted_expiry_height,
    }))
}

fn build_evidence(
    direction: SwapDirection,
    funding: &FundingEvidence,
    claim: &ClaimEvidence,
    accepted_expiry_height: Option<u64>,
) -> Result<CombinedSwapEvidence, SwapError> {
    if !claim.preimage_verified || funding.confirmations == 0 {
        return Err(SwapError::Chain(
            "swap evidence is not confirmed and witness-bound".to_owned(),
        ));
    }

    Ok(CombinedSwapEvidence {
        direction,
        funding_txid: funding.txid.clone(),
        claim_txid: claim.txid.clone(),
        claim_confirmations: claim.confirmations,
        invoice_settled: true,
        payer_succeeded: true,
        preimage_verified: true,
        accepted_expiry_height,
    })
}

fn cancel_if_accepted<L: LightningAdapter>(receiver: &L, hash: &[u8; 32], amount_sats: u64) {
    let Ok(invoice) = receiver.observe_accepted(*hash, amount_sats) else {
        return;
    };

    if invoice.state != InvoiceState::Accepted {
        return;
    }

    let _ = receiver.cancel_hold(CancelHoldRequest {
        payment_hash: *hash,
        deadline: Some(SystemTime::now() + Duration::from_secs(60)),
    });
}

fn read_funding(path: &Path) -> Result<Option<FundingOutbox>, SwapError> {
    read_outbox(path)
}

fn persist_funding(path: &Path, prepared: PreparedTransaction) -> Result<FundingOutbox, SwapError> {
    if prepared.outbox.kind != OutboxKind::Funding {
        return Err(SwapError::Durability(
            "funding adapter returned the wrong outbox kind".to_owned(),
        ));
    }

    let mut outbox = prepared.outbox;
    outbox.funding_vout = prepared.funding_vout;
    let _ = write_outbox(path, &outbox)?;

    Ok(outbox)
}

fn persist_spend(path: &Path, prepared: PreparedTransaction) -> Result<FundingOutbox, SwapError> {
    if prepared.outbox.kind != OutboxKind::Claim && prepared.outbox.kind != OutboxKind::Refund {
        return Err(SwapError::Durability(
            "spend adapter returned a funding outbox".to_owned(),
        ));
    }

    let outbox = prepared.outbox;
    let _ = write_outbox(path, &outbox)?;

    Ok(outbox)
}

fn validate_funding_outbox(outbox: FundingOutbox) -> Result<FundingOutbox, SwapError> {
    if outbox.kind != OutboxKind::Funding {
        return Err(SwapError::RecoveryMismatch(
            "funding outbox has a non-funding kind".to_owned(),
        ));
    }

    if outbox.funding_vout.is_none() {
        return Err(SwapError::RecoveryMismatch(
            "funding outbox lacks its HTLC output index".to_owned(),
        ));
    }

    Ok(outbox)
}

fn validate_claim_outbox(outbox: FundingOutbox) -> Result<FundingOutbox, SwapError> {
    validate_spend_outbox_kind(outbox, OutboxKind::Claim)
}

fn validate_spend_outbox(
    outbox: FundingOutbox,
    kind: OutboxKind,
    funding: &FundingEvidence,
) -> Result<FundingOutbox, SwapError> {
    let outbox = validate_spend_outbox_kind(outbox, kind)?;

    if outbox.funding_txid.as_deref() != Some(funding.txid.as_str())
        || outbox.funding_vout != Some(funding.vout)
    {
        return Err(SwapError::RecoveryMismatch(
            "spend outbox does not bind the expected funding output".to_owned(),
        ));
    }

    Ok(outbox)
}

fn validate_spend_outbox_kind(
    outbox: FundingOutbox,
    kind: OutboxKind,
) -> Result<FundingOutbox, SwapError> {
    if outbox.kind != kind || outbox.funding_txid.is_none() || outbox.funding_vout.is_none() {
        return Err(SwapError::RecoveryMismatch(
            "spend outbox is incomplete or has the wrong kind".to_owned(),
        ));
    }

    Ok(outbox)
}
