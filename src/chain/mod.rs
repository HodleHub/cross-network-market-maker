//! Bitcoin and Elements regtest HTLC adapters.

mod bitcoin;
mod contract;
mod htlc_script;
mod liquid;
mod observation;
pub mod types;

pub use bitcoin::{
    BitcoinFundingArgs, BitcoinSpendArgs, build_bitcoin_funding_transaction,
    build_bitcoin_htlc_spend, extract_claim_preimage, sign_bitcoin_funding_transaction,
    sign_bitcoin_htlc_spend,
};
pub use contract::{create_htlc_contract, validate_htlc_contract};
pub use liquid::{
    LiquidFundingArgs, LiquidSpendArgs, build_liquid_funding_transaction, build_liquid_htlc_spend,
    claim_liquid_htlc, extract_liquid_claim_preimage, refund_liquid_htlc,
    sign_liquid_funding_transaction, sign_liquid_htlc_spend,
};
pub use observation::{
    BroadcastBitcoinArgs, BroadcastEvidence, BroadcastLiquidArgs, ClaimObservation,
    InspectBitcoinHtlcArgs, InspectLiquidHtlcArgs, ObserveBitcoinClaimArgs, ObserveLiquidClaimArgs,
    broadcast_bitcoin_transaction, broadcast_liquid_transaction, inspect_bitcoin_htlc_output,
    inspect_liquid_htlc_output, observe_bitcoin_claim, observe_liquid_claim, parse_rpc_coin_amount,
};
pub use types::{
    AssetId, BitcoinUtxo, Chain, ChainError, ClaimPreimage, FundingEvidence, HtlcContract,
    LiquidUtxo, SignedTransaction, SpendKind,
};
