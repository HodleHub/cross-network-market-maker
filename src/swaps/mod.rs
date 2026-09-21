//! Durable, regtest-only swap preparation and timing primitives.

mod adapter;
mod error;
mod liquid;
mod orchestrator;
mod outbox;
mod recovery;
pub mod regtest;
mod timing;

pub use adapter::{
    BroadcastEvidence, ClaimEvidence, PrepareFundingRequest, PrepareSpendRequest,
    PreparedTransaction, RefundEvidence, SpendKeyMaterial, SwapChainAdapter, funding_outbox,
};
pub use error::SwapError;
pub use liquid::LiquidRpcAdapter;
pub use orchestrator::{
    CombinedSwapEvidence, ForwardSwapRequest, NoopSwapObserver, RefundSwapEvidence,
    ReverseSwapRequest, SwapEncryptionKey, SwapObserver, SwapPaths, SwapPhase, SwapTerms,
    SwapTermsDefaults, refund_funded_swap, refund_funded_swap_with_observer, run_forward_swap,
    run_reverse_swap,
};
pub use outbox::{FundingOutbox, OutboxKind, read_outbox, write_outbox};
pub use recovery::{
    PrepareRecoveryRequest, PreparedSwapSession, SwapDirection, SwapKeyMaterial,
    SwapRecoveryRecord, generate_swap_secrets, prepare_or_load_recovery, read_recovery,
};
pub use timing::{
    BITCOIN_BLOCK_SECONDS, CLAIM_MARGIN_SECONDS, LIQUID_BLOCK_SECONDS, TimingEvidence,
    validate_forward_timing, validate_reverse_timing,
};
