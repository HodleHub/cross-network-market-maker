use thiserror::Error;

pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid amount: {0}")]
    InvalidAmount(String),
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("invalid hash commitment: {0}")]
    InvalidHash(String),
    #[error("invalid network: {0}")]
    InvalidNetwork(String),
    #[error("unsupported network: {0}")]
    UnsupportedNetwork(String),
    #[error("unsupported route: {source_endpoint} -> {destination_endpoint}")]
    UnsupportedRoute {
        source_endpoint: String,
        destination_endpoint: String,
    },
    #[error("signature verification failed: {0}")]
    Signature(String),
    #[error("quote rejected: {0}")]
    QuoteRejected(String),
    #[error("deadline policy rejected: {0}")]
    DeadlineRejected(String),
    #[error("invalid state transition: {0}")]
    InvalidState(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("adapter error: {0}")]
    Adapter(String),
}
