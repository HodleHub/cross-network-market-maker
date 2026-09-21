//! Typed errors for native Lightning unilateral exit qualification.

use thiserror::Error;

/// Errors returned while parsing, driving, or proving a native exit.
#[derive(Debug, Error)]
pub enum ExitError {
    /// A response or transaction did not satisfy the required shape.
    #[error("invalid exit data: {0}")]
    InvalidData(String),
    /// The LND or chain endpoint was not the local regtest fixture.
    #[error("exit endpoint is not qualified regtest: {0}")]
    WrongNetwork(String),
    /// A CSV or timeout proof did not bind the target output.
    #[error("exit proof failed: {0}")]
    Proof(String),
    /// HTTP or TLS request failed.
    #[error("exit request failed: {0}")]
    Request(String),
    /// Raw Bitcoin transaction parsing failed.
    #[error("exit transaction parsing failed: {0}")]
    Transaction(String),
}
