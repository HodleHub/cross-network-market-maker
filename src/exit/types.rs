//! Public native-exit records and recovery metadata.

use bitcoin::ScriptBuf;
use serde::{Deserialize, Serialize};

/// A channel point represented by its funding transaction and output index.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChannelPoint {
    /// Funding transaction identifier.
    pub funding_txid: String,
    /// Funding output index.
    pub funding_vout: u32,
    /// Original LND `channel_point` text.
    pub serialized: String,
}

/// An active or inactive channel candidate for a unilateral exit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExitChannel {
    /// Funding channel point.
    pub channel_point: ChannelPoint,
    /// Remote node identity key.
    pub remote_pubkey: String,
    /// Channel capacity in satoshis.
    pub capacity_sats: u64,
    /// Payer's current local balance in satoshis.
    pub local_balance_sats: u64,
    /// Whether LND reports the channel active.
    pub active: bool,
    /// HTLCs currently carried by this channel.
    pub pending_htlcs: Vec<ExitHtlc>,
}

/// A pending channel HTLC before force close.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExitHtlc {
    /// Payment hash commitment.
    pub hash_lock: [u8; 32],
    /// Offered HTLC amount in satoshis.
    pub amount_sats: u64,
    /// Whether the HTLC is incoming to the node.
    pub incoming: bool,
    /// Absolute CLTV expiry height.
    pub expiration_height: u64,
}

/// LND's pending force-close record with signed maturity fields.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingForceClose {
    /// Channel point being closed.
    pub channel_point: ChannelPoint,
    /// Commitment transaction identifier.
    pub closing_txid: String,
    /// Limbo balance reported by LND.
    pub limbo_balance_sats: u64,
    /// Commitment output maturity height; can be in the past after polling delay.
    pub maturity_height: i64,
    /// Signed blocks remaining until maturity.
    pub blocks_til_maturity: i64,
    /// HTLC second-stage records reported by LND.
    pub pending_htlcs: Vec<PendingHtlc>,
}

/// LND's pending HTLC second-stage record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingHtlc {
    /// Canonical point reported by LND; it may be replaced by a batched timeout transaction.
    pub outpoint: ChannelPoint,
    /// HTLC amount in satoshis.
    pub amount_sats: u64,
    /// Whether the HTLC is incoming to the node.
    pub incoming: bool,
    /// Reported second-stage maturity height.
    pub maturity_height: i64,
    /// Signed blocks remaining until second-stage maturity.
    pub blocks_til_maturity: i64,
    /// LND pending HTLC stage.
    pub stage: u32,
}

/// Exact observed timeout transaction input and its paired SIGHASH_SINGLE output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HtlcTimeoutProof {
    /// Timeout transaction identifier.
    pub timeout_txid: String,
    /// Commitment transaction identifier.
    pub commitment_txid: String,
    /// Commitment output index spent by the timeout transaction.
    pub commitment_vout: u32,
    /// Matching input index in the timeout transaction.
    pub timeout_input_index: usize,
    /// Paired output index selected by SIGHASH_SINGLE.
    pub timeout_output_index: usize,
    /// Paired timeout output amount in satoshis.
    pub timeout_amount_sats: u64,
    /// Script of the paired delayed timeout output.
    pub timeout_output_script: ScriptBuf,
    /// Timeout transaction absolute lock height.
    pub lock_time: u32,
    /// Sequence used by the timeout input.
    pub sequence: u32,
}

/// Exact CSV sweep proof for one delayed exit output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CsvSpendProof {
    /// Sweep transaction identifier.
    pub sweep_txid: String,
    /// Delayed source transaction identifier.
    pub source_txid: String,
    /// Delayed source output index.
    pub source_vout: u32,
    /// Matching sweep input index.
    pub sweep_input_index: usize,
    /// Observed BIP68 sequence.
    pub sequence: u32,
    /// Required relative block delay.
    pub required_csv: u16,
    /// Source confirmation height.
    pub source_height: u64,
    /// Sweep confirmation height.
    pub sweep_height: u64,
    /// Total value sent to caller-owned output scripts.
    pub recovered_sats: u64,
}

/// Caller-owned output match used to prove LND swept into its wallet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedOutputSet {
    /// Output scripts controlled by the payer wallet.
    pub scripts: Vec<ScriptBuf>,
    /// Total amount matched across those scripts.
    pub amount_sats: u64,
}

/// Public accounting for timeout and CSV-stage fees.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExitRecoveryAccounting {
    /// Original HTLC principal.
    pub principal_sats: u64,
    /// Timeout stage output amount before its fee.
    pub timeout_output_sats: u64,
    /// Fee assigned to the timeout stage, including a wallet sponsor fee when conservative.
    pub timeout_stage_fee_sats: u64,
    /// Final CSV sweep amount sent to owned scripts.
    pub final_sweep_sats: u64,
    /// Final CSV transaction fee assigned to the target output.
    pub final_sweep_fee_sats: u64,
    /// Attributable final value capped to the original target principal.
    pub attributable_final_sweep_sats: u64,
    /// Principal recovered after both fee stages.
    pub net_recovered_sats: u64,
}

/// A wallet transaction output reported by LND with its ownership flag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalletTransactionOutput {
    /// Transaction identifier containing the output.
    pub txid: String,
    /// Output index within the transaction.
    pub output_index: u32,
    /// Address encoded by LND for the output.
    pub address: String,
    /// Whether LND marks this output as owned by its wallet.
    pub is_our_address: bool,
}
