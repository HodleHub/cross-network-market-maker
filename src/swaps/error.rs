use thiserror::Error;

/// Errors raised while preparing or recovering a swap.
#[derive(Debug, Error)]
pub enum SwapError {
    /// A caller supplied a malformed or inconsistent swap value.
    #[error("invalid swap input: {0}")]
    InvalidInput(String),
    /// The durable recovery or outbox record could not be read or written.
    #[error("swap durability error: {0}")]
    Durability(String),
    /// The saved session does not match the requested replay.
    #[error("swap recovery mismatch: {0}")]
    RecoveryMismatch(String),
    /// The selected timing policy does not leave enough cross-network headroom.
    #[error("swap timing rejected: {0}")]
    Timing(String),
    /// An LND adapter operation failed.
    #[error("lightning adapter error: {0}")]
    Lightning(String),
    /// A chain adapter operation failed.
    #[error("chain adapter error: {0}")]
    Chain(String),
    /// A side effect was attempted while the durable state was ambiguous.
    #[error("swap state is unknown: {0}")]
    Unknown(String),
}
