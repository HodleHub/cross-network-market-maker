use std::time::{Duration, SystemTime};

/// The only Lightning network accepted by this research adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LightningNetwork {
    /// Bitcoin regtest as exposed by LND.
    Regtest,
}

/// TLS-pinned local LND REST client configuration.
pub struct LndRestConfig {
    /// HTTPS loopback endpoint, normally `https://127.0.0.1:<port>`.
    pub base_url: String,
    /// PEM certificate used as the sole trust anchor for the endpoint.
    pub tls_certificate_pem: Vec<u8>,
    /// Hexadecimal admin macaroon loaded from the dedicated runtime directory.
    pub macaroon_hex: String,
    /// Default bound for one REST request.
    pub request_timeout: Duration,
    /// Network qualification requested by the caller.
    pub network: LightningNetwork,
}

/// Information required to create a private hold invoice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HoldInvoiceRequest {
    /// 32-byte payment hash commitment.
    pub payment_hash: [u8; 32],
    /// Exact invoice amount in satoshis.
    pub amount_sats: u64,
    /// Relative CLTV delta requested from LND.
    pub cltv_expiry: u32,
    /// Optional private operator memo.
    pub memo: Option<String>,
}

/// State reported for a hold invoice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvoiceState {
    /// Invoice has not accepted an HTLC.
    Open,
    /// Invoice has one or more accepted HTLCs.
    Accepted,
    /// Invoice was settled with its preimage.
    Settled,
    /// Invoice was canceled by the receiver or expired.
    Canceled,
    /// LND returned an unrecognized state.
    Unknown,
}

/// An invoice and the accepted HTLC evidence observed for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HoldInvoice {
    /// Committed payment hash.
    pub payment_hash: [u8; 32],
    /// Signed BOLT11 request, when LND returns it.
    pub payment_request: Option<String>,
    /// Invoice state.
    pub state: InvoiceState,
    /// Original invoice amount in satoshis.
    pub amount_sats: u64,
    /// Requested relative CLTV delta.
    pub cltv_expiry: u32,
    /// LND add index represented as decimal text.
    pub add_index: String,
    /// Sum of accepted HTLC amounts, if present and integral in satoshis.
    pub accepted_amount_sats: Option<u64>,
    /// Earliest accepted HTLC absolute expiry height, if present.
    pub accepted_expiry_height: Option<u64>,
}

/// Node tip and chain qualification returned by `getinfo`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LightningTip {
    /// LND identity public key in hexadecimal form.
    pub identity_pubkey: String,
    /// LND alias, retained for sanitized public evidence only.
    pub alias: String,
    /// Bitcoin block height observed by LND.
    pub block_height: u64,
    /// Whether LND reports itself synchronized.
    pub synced_to_chain: bool,
}

/// Payment lifecycle state from router send or tracking.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentState {
    /// Payment is still being routed.
    InFlight,
    /// Payment succeeded and yielded a verified preimage.
    Succeeded,
    /// Payment reached a terminal failure.
    Failed,
    /// The request ended before the payment state was known.
    Unknown,
}

/// Sanitized payment observation. A preimage is present only for a verified success.
#[derive(Clone, Eq, PartialEq)]
pub struct PaymentObservation {
    /// Committed payment hash.
    pub payment_hash: [u8; 32],
    /// Observed router state.
    pub state: PaymentState,
    /// Optional LND failure reason.
    pub failure_reason: Option<String>,
    /// Optional routing fee in satoshis.
    pub fee_sats: Option<u64>,
    /// Verified payment preimage; absent for every non-success state.
    pub payment_preimage: Option<[u8; 32]>,
}

impl std::fmt::Debug for PaymentObservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PaymentObservation")
            .field("payment_hash", &hex::encode(self.payment_hash))
            .field("state", &self.state)
            .field("failure_reason", &self.failure_reason)
            .field("fee_sats", &self.fee_sats)
            .field(
                "payment_preimage",
                &self.payment_preimage.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// BOLT11 invoice fields bound before a payment POST is allowed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvoiceBinding {
    /// Payment hash encoded by the invoice.
    pub payment_hash: [u8; 32],
    /// Exact invoice amount in satoshis.
    pub amount_sats: u64,
}

/// Bounded router payment request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentRequest {
    /// Signed BOLT11 request.
    pub payment_request: String,
    /// Expected payment hash supplied by the signed quote.
    pub payment_hash: [u8; 32],
    /// Expected invoice amount in satoshis.
    pub expected_amount_sats: u64,
    /// Maximum routing fee in satoshis.
    pub fee_limit_sats: u64,
    /// Maximum final CLTV limit accepted for this POC.
    pub cltv_limit: u32,
    /// Bound on the router call. Timeout leaves the payment unknown.
    pub timeout: Duration,
}

/// Successful hold settlement evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettlementReceipt {
    /// Payment hash of the settled invoice.
    pub payment_hash: [u8; 32],
}

/// Successful hold cancellation evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationReceipt {
    /// Payment hash of the canceled invoice.
    pub payment_hash: [u8; 32],
}

/// Deadline guard for cooperative hold cancellation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelHoldRequest {
    /// Payment hash of the hold invoice.
    pub payment_hash: [u8; 32],
    /// Cancellation must be submitted before this instant.
    pub deadline: Option<SystemTime>,
}

/// Interface consumed by swap orchestration and resumable recovery.
pub trait LightningAdapter {
    /// Return a qualified Bitcoin regtest tip.
    fn current_tip(&self) -> Result<LightningTip, super::error::LightningError>;
    /// Create a private hold invoice.
    fn create_hold_invoice(
        &self,
        request: HoldInvoiceRequest,
    ) -> Result<HoldInvoice, super::error::LightningError>;
    /// Observe an accepted HTLC with exact amount and expiry evidence.
    fn observe_accepted(
        &self,
        payment_hash: [u8; 32],
        expected_amount_sats: u64,
    ) -> Result<HoldInvoice, super::error::LightningError>;
    /// Read an invoice in any terminal or in-flight state for recovery decisions.
    fn lookup_invoice(
        &self,
        payment_hash: [u8; 32],
    ) -> Result<HoldInvoice, super::error::LightningError>;
    /// Initiate a bounded payment.
    fn pay(
        &self,
        request: PaymentRequest,
    ) -> Result<PaymentObservation, super::error::LightningError>;
    /// Track a payment by its committed hash.
    fn track_by_hash(
        &self,
        payment_hash: [u8; 32],
        timeout: Duration,
    ) -> Result<PaymentObservation, super::error::LightningError>;
    /// Settle an accepted hold with a verified preimage.
    fn settle_hold(
        &self,
        payment_hash: [u8; 32],
        preimage: [u8; 32],
    ) -> Result<SettlementReceipt, super::error::LightningError>;
    /// Cooperatively cancel an accepted hold before its deadline.
    fn cancel_hold(
        &self,
        request: CancelHoldRequest,
    ) -> Result<CancellationReceipt, super::error::LightningError>;
}
