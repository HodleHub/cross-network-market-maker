use thiserror::Error;

/// Typed errors returned by the regtest-only LND adapter.
#[derive(Debug, Error)]
pub enum LightningError {
    /// Adapter configuration is outside the local regtest profile.
    #[error("invalid LND configuration: {0}")]
    InvalidConfiguration(String),
    /// Endpoint is not HTTPS loopback.
    #[error("LND endpoint must be HTTPS loopback")]
    EndpointNotLocalTls,
    /// Certificate could not be parsed or installed.
    #[error("LND TLS certificate is invalid: {0}")]
    InvalidCertificate(String),
    /// Macaroon is absent or not hexadecimal.
    #[error("LND macaroon is invalid")]
    InvalidMacaroon,
    /// HTTP transport failed before a terminal payment result was known.
    #[error("LND request failed: {0}")]
    Transport(String),
    /// Request deadline elapsed before a terminal response.
    #[error("LND request timed out; payment state remains unknown")]
    RequestTimeout,
    /// HTTP endpoint returned a non-success response.
    #[error("LND HTTP {status}: {message}")]
    Http { status: u16, message: String },
    /// Response body was not valid JSON.
    #[error("LND JSON response is invalid: {0}")]
    InvalidJson(String),
    /// JSON response was missing a required field or had an invalid value.
    #[error("LND response is invalid: {0}")]
    InvalidResponse(String),
    /// LND reported a different chain.
    #[error("LND chain is not bitcoin")]
    WrongChain,
    /// LND reported a non-regtest network.
    #[error("LND network is not regtest")]
    WrongNetwork,
    /// LND has not synchronized to the Bitcoin regtest tip.
    #[error("LND is not synchronized to regtest")]
    NotSynced,
    /// BOLT11 text could not be decoded.
    #[error("BOLT11 invoice is invalid: {0}")]
    InvalidInvoice(String),
    /// Invoice uses a network other than Bitcoin regtest.
    #[error("BOLT11 invoice is not for Bitcoin regtest")]
    InvoiceWrongNetwork,
    /// BOLT11 invoice expiry elapsed before the payment attempt.
    #[error("BOLT11 invoice has expired")]
    InvoiceExpired,
    /// Invoice hash differs from the signed quote commitment.
    #[error("BOLT11 payment hash does not match the quote")]
    InvoiceHashMismatch,
    /// Invoice amount differs from the signed quote amount.
    #[error("BOLT11 amount does not match the quote")]
    InvoiceAmountMismatch,
    /// A payment hash is not exactly 32 bytes.
    #[error("payment hash must contain 32 bytes")]
    PaymentHashInvalid,
    /// A preimage is not exactly 32 bytes.
    #[error("payment preimage must contain 32 bytes")]
    PreimageInvalid,
    /// Preimage does not hash to the committed payment hash.
    #[error("payment preimage does not match the hash commitment")]
    PreimageMismatch,
    /// LND returned a successful payment without a valid preimage.
    #[error("successful LND payment did not contain a verified preimage")]
    PreimageMissing,
    /// Hold cancellation was attempted outside its safe state.
    #[error("hold invoice cannot be canceled in its current state")]
    UnsafeCancelState,
    /// Hold cancellation deadline has elapsed.
    #[error("hold cancellation deadline has passed")]
    CancelDeadlinePassed,
    /// Hold settlement was attempted outside its safe state.
    #[error("hold invoice cannot be settled in its current state")]
    UnsafeSettleState,
    /// Numeric parameter exceeds the bounded POC policy.
    #[error("LND parameter is outside the bounded regtest policy")]
    ParameterOutOfRange,
}
