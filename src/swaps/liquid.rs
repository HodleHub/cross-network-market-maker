//! Concrete Elements regtest adapter for explicit TEST-DEPIX HTLC swaps.

use std::str::FromStr;

use bitcoin::ScriptBuf;
use bitcoin::key::CompressedPublicKey;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde_json::{Value, json};

use crate::chain::{
    AssetId, BroadcastLiquidArgs, FundingEvidence, HtlcContract, InspectLiquidHtlcArgs,
    LiquidFundingArgs, LiquidSpendArgs, LiquidUtxo, ObserveLiquidClaimArgs, SpendKind,
    broadcast_liquid_transaction, build_liquid_funding_transaction, build_liquid_htlc_spend,
    inspect_liquid_htlc_output, observe_liquid_claim, sign_liquid_funding_transaction,
    sign_liquid_htlc_spend,
};
use crate::rpc::RpcClient;

use super::adapter::{
    BroadcastEvidence, ClaimEvidence, PrepareFundingRequest, PrepareSpendRequest,
    PreparedTransaction, RefundEvidence, SwapChainAdapter,
};
use super::error::SwapError;
use super::outbox::{FundingOutbox, OutboxKind};

const MIN_CONFIRMATIONS: u32 = 1;
const MIN_REFUND_HEADROOM: u64 = 1;
const LIST_UNSPENT_MIN: u64 = 1;
const LIST_UNSPENT_MAX: u64 = 9_999_999;

/// Wallet backed Elements adapter used only with the dedicated regtest wallet.
#[derive(Clone, Debug)]
pub struct LiquidRpcAdapter {
    rpc: RpcClient,
    wallet: String,
    fee_asset_id: AssetId,
}

impl LiquidRpcAdapter {
    /// Creates an adapter for one authenticated loopback Elements wallet.
    pub fn new(
        rpc: RpcClient,
        wallet: impl Into<String>,
        fee_asset_id: AssetId,
    ) -> Result<Self, SwapError> {
        if fee_asset_id == AssetId::Bitcoin {
            return Err(SwapError::InvalidInput(
                "Elements fee asset must be explicit".to_owned(),
            ));
        }

        let wallet = wallet.into();

        if wallet.is_empty() {
            return Err(SwapError::InvalidInput(
                "Elements wallet name must not be empty".to_owned(),
            ));
        }

        let rpc = rpc
            .scoped_to_wallet(&wallet)
            .map_err(|error| SwapError::Chain(error.to_string()))?;

        Ok(Self {
            rpc,
            wallet,
            fee_asset_id,
        })
    }

    /// Returns the wallet name used by this adapter.
    pub fn wallet(&self) -> &str {
        &self.wallet
    }

    /// Creates two explicit payment-asset and two explicit fee-asset wallet outputs.
    ///
    /// Callers must mine one Elements block and inspect the outputs before using them
    /// as HTLC inputs. The single sendmany call prevents fee coin selection from
    /// consuming an explicit output prepared by a concurrent request.
    pub fn prepare_explicit_liquidity(
        &self,
        asset_id: AssetId,
        fee_asset_id: AssetId,
        amount_sats: u64,
    ) -> Result<String, SwapError> {
        if asset_id == AssetId::Bitcoin || fee_asset_id == AssetId::Bitcoin || amount_sats == 0 {
            return Err(SwapError::InvalidInput(
                "explicit liquidity requires two asset ids and a positive amount".to_owned(),
            ));
        }

        let asset_addresses = [
            self.unconfidential_address()?,
            self.unconfidential_address()?,
        ];
        let fee_addresses = [
            self.unconfidential_address()?,
            self.unconfidential_address()?,
        ];
        let all_addresses = asset_addresses
            .iter()
            .chain(fee_addresses.iter())
            .cloned()
            .collect::<Vec<_>>();
        let mut amounts = serde_json::Map::new();
        let mut output_assets = serde_json::Map::new();

        for address in &all_addresses {
            amounts.insert(address.clone(), Value::String(decimal_sats(amount_sats)));
        }

        for address in &asset_addresses {
            output_assets.insert(address.clone(), Value::String(asset_id.to_hex()));
        }

        for address in &fee_addresses {
            output_assets.insert(address.clone(), Value::String(fee_asset_id.to_hex()));
        }

        let result: Value = self.call(
            "sendmany",
            &[
                json!(""),
                Value::Object(amounts),
                json!(1),
                json!("cross-network-market-maker-liquidity"),
                json!([]),
                json!(false),
                json!(1),
                json!("ECONOMICAL"),
                Value::Object(output_assets),
                json!(true),
            ],
        )?;

        result
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                result
                    .get("txid")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .ok_or_else(|| SwapError::Chain("Elements sendmany did not return a txid".to_owned()))
    }

    /// Mines one Elements regtest block to confirm a just-broadcast transaction.
    pub fn mine_one_block(&self) -> Result<(), SwapError> {
        self.mine_blocks(1)
    }

    /// Mines a bounded number of Elements regtest blocks for timeout qualification.
    pub fn mine_blocks(&self, blocks: u64) -> Result<(), SwapError> {
        if blocks == 0 {
            return Ok(());
        }

        let address: String = self.call("getnewaddress", &[])?;
        let _: Vec<String> = self.call("generatetoaddress", &[json!(blocks), json!(address)])?;

        Ok(())
    }

    /// Verifies that a refund returned the exact explicit asset to its destination key.
    pub fn verify_refund_output(
        &self,
        txid: &str,
        asset_id: AssetId,
        amount_sats: u64,
        destination_public_key: &PublicKey,
    ) -> Result<(), SwapError> {
        let transaction: Value = self.call("getrawtransaction", &[json!(txid), json!(true)])?;
        let expected_script = p2wpkh_script(destination_public_key)?;
        let expected_script_hex = hex::encode(expected_script.as_bytes());
        let expected_asset = asset_id.to_hex();
        let outputs = transaction
            .get("vout")
            .and_then(Value::as_array)
            .ok_or_else(|| SwapError::Chain("Elements refund outputs are missing".to_owned()))?;
        let found = outputs.iter().any(|output| {
            let output_asset = output.get("asset").and_then(Value::as_str);
            let amount = output.get("value").and_then(|value| parse_sats(value).ok());
            let script = output.get("scriptPubKey").and_then(|value| {
                value
                    .as_str()
                    .or_else(|| value.get("hex").and_then(Value::as_str))
            });

            output_asset == Some(expected_asset.as_str())
                && amount == Some(amount_sats)
                && script == Some(expected_script_hex.as_str())
        });

        if !found {
            return Err(SwapError::Chain(
                "refund did not return the expected explicit asset output".to_owned(),
            ));
        }

        Ok(())
    }

    fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &[Value],
    ) -> Result<T, SwapError> {
        self.rpc
            .call(method, params)
            .map_err(|error| SwapError::Chain(error.to_string()))
    }

    fn ensure_regtest(&self) -> Result<u64, SwapError> {
        let info: Value = self.call("getblockchaininfo", &[])?;
        let chain = info
            .get("chain")
            .and_then(Value::as_str)
            .ok_or_else(|| SwapError::Chain("Elements chain name is missing".to_owned()))?;

        if chain != "elementsregtest" && chain != "liquidregtest" {
            return Err(SwapError::Chain(format!(
                "Elements RPC is on {chain}, expected Elements regtest"
            )));
        }

        info.get("blocks")
            .and_then(Value::as_u64)
            .ok_or_else(|| SwapError::Chain("Elements block height is missing".to_owned()))
    }

    fn wallet_address(&self) -> Result<(String, ScriptBuf), SwapError> {
        let address: String = self.call("getnewaddress", &[])?;
        let info: Value = self.call("getaddressinfo", &[json!(address)])?;
        let script_hex = info
            .get("scriptPubKey")
            .and_then(|value| {
                value
                    .as_str()
                    .or_else(|| value.get("hex").and_then(Value::as_str))
            })
            .ok_or_else(|| SwapError::Chain("wallet address script is missing".to_owned()))?;
        let script = parse_script(script_hex, "wallet script")?;

        Ok((address, script))
    }

    fn unconfidential_address(&self) -> Result<String, SwapError> {
        let address: String = self.call("getnewaddress", &[])?;
        let info: Value = self.call("getaddressinfo", &[json!(address)])?;
        let unconfidential = info
            .get("unconfidential")
            .or_else(|| info.get("unconfidential_address"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SwapError::Chain(
                    "Elements wallet did not return an unconfidential address".to_owned(),
                )
            })?;

        Ok(unconfidential.to_owned())
    }

    fn select_utxo(&self, asset_id: AssetId, minimum_sats: u64) -> Result<WalletUtxo, SwapError> {
        let outputs: Vec<Value> = self.call(
            "listunspent",
            &[json!(LIST_UNSPENT_MIN), json!(LIST_UNSPENT_MAX)],
        )?;

        outputs
            .into_iter()
            .filter_map(|output| self.parse_utxo(output).ok())
            .find(|output| {
                output.utxo.asset_id == asset_id
                    && output.utxo.amount_sats >= minimum_sats
                    && output.utxo.confirmations >= MIN_CONFIRMATIONS
            })
            .ok_or_else(|| {
                SwapError::Chain(format!(
                    "no explicit Elements UTXO is available for {} with {} sats",
                    asset_id.to_hex(),
                    minimum_sats
                ))
            })
    }

    fn parse_utxo(&self, value: Value) -> Result<WalletUtxo, SwapError> {
        if has_commitment(&value, "amountcommitment") || has_commitment(&value, "assetcommitment") {
            return Err(SwapError::Chain(
                "confidential wallet UTXO cannot fund an explicit HTLC".to_owned(),
            ));
        }

        let asset_text = value
            .get("asset")
            .and_then(Value::as_str)
            .ok_or_else(|| SwapError::Chain("wallet UTXO asset is missing".to_owned()))?;
        let asset_id =
            AssetId::from_hex(asset_text).map_err(|error| SwapError::Chain(error.to_string()))?;
        let amount_value = value
            .get("amount")
            .ok_or_else(|| SwapError::Chain("wallet UTXO amount is missing".to_owned()))?;
        let amount_sats = parse_sats(amount_value)?;
        let confirmations = value
            .get("confirmations")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| SwapError::Chain("wallet UTXO confirmations are missing".to_owned()))?;
        let address = value
            .get("address")
            .and_then(Value::as_str)
            .ok_or_else(|| SwapError::Chain("wallet UTXO address is missing".to_owned()))?
            .to_owned();
        let script_hex = value
            .get("scriptPubKey")
            .and_then(|script| {
                script
                    .as_str()
                    .or_else(|| script.get("hex").and_then(Value::as_str))
            })
            .ok_or_else(|| SwapError::Chain("wallet UTXO script is missing".to_owned()))?;
        let script_pubkey = parse_script(script_hex, "wallet UTXO script")?;
        let txid = value
            .get("txid")
            .and_then(Value::as_str)
            .ok_or_else(|| SwapError::Chain("wallet UTXO txid is missing".to_owned()))?
            .to_owned();
        let vout = value
            .get("vout")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| SwapError::Chain("wallet UTXO vout is missing".to_owned()))?;
        let private_key = self.dump_private_key(&address)?;

        validate_wallet_script(&script_pubkey, &private_key)?;

        Ok(WalletUtxo {
            utxo: LiquidUtxo {
                txid,
                vout,
                asset_id,
                amount_sats,
                script_pubkey,
                confirmations,
            },
            private_key,
        })
    }

    fn dump_private_key(&self, address: &str) -> Result<SecretKey, SwapError> {
        let encoded: String = self.call("dumpprivkey", &[json!(address)])?;
        let private_key = bitcoin::PrivateKey::from_wif(&encoded)
            .map_err(|_| SwapError::Chain("wallet returned an invalid private key".to_owned()))?;

        if !private_key.compressed {
            return Err(SwapError::Chain(
                "wallet returned an uncompressed private key".to_owned(),
            ));
        }

        Ok(private_key.inner)
    }

    fn make_funding(
        &self,
        request: &PrepareFundingRequest,
    ) -> Result<PreparedTransaction, SwapError> {
        validate_funding_request(request)?;

        if request.fee_asset_id != self.fee_asset_id {
            return Err(SwapError::InvalidInput(
                "funding fee asset differs from the adapter wallet fee asset".to_owned(),
            ));
        }

        let payment = self.select_utxo(request.contract.asset_id, request.amount_sats)?;
        let fee = self.select_utxo(request.fee_asset_id, request.fee_sats)?;

        if same_outpoint(&payment.utxo, &fee.utxo) {
            return Err(SwapError::Chain(
                "payment and fee UTXOs must be distinct".to_owned(),
            ));
        }

        let (_, payment_change_script) = self.wallet_address()?;
        let (_, fee_change_script) = self.wallet_address()?;
        let payment_change = payment
            .utxo
            .amount_sats
            .checked_sub(request.amount_sats)
            .ok_or_else(|| SwapError::Chain("payment UTXO is too small".to_owned()))?;
        let fee_change = fee
            .utxo
            .amount_sats
            .checked_sub(request.fee_sats)
            .ok_or_else(|| SwapError::Chain("fee UTXO is too small".to_owned()))?;
        let args = LiquidFundingArgs {
            payment_input: payment.utxo.clone(),
            fee_input: fee.utxo.clone(),
            contract: request.contract.clone(),
            htlc_amount_sats: request.amount_sats,
            payment_change_script,
            payment_change_amount_sats: payment_change,
            fee_change_script,
            fee_change_amount_sats: fee_change,
            fee_sats: request.fee_sats,
            fee_asset_id: request.fee_asset_id,
        };
        let unsigned = build_liquid_funding_transaction(&args)
            .map_err(|error| SwapError::Chain(error.to_string()))?;
        let signed = sign_liquid_funding_transaction(
            unsigned,
            &payment.utxo,
            &fee.utxo,
            &payment.private_key,
            &fee.private_key,
        )
        .map_err(|error| SwapError::Chain(error.to_string()))?;
        let outbox = FundingOutbox {
            kind: OutboxKind::Funding,
            txid: signed.txid,
            raw_hex: signed.raw_hex,
            funding_txid: None,
            funding_vout: None,
        };

        Ok(PreparedTransaction {
            outbox,
            funding_vout: Some(0),
            asset_id: request.contract.asset_id,
            amount_sats: request.amount_sats,
        })
    }

    fn make_spend(&self, request: &PrepareSpendRequest) -> Result<PreparedTransaction, SwapError> {
        validate_spend_request(request)?;

        if request.fee_asset_id != self.fee_asset_id {
            return Err(SwapError::InvalidInput(
                "spend fee asset differs from the adapter wallet fee asset".to_owned(),
            ));
        }

        let fee = self.select_utxo(request.fee_asset_id, request.fee_sats)?;

        if fee.utxo.txid == request.funding.txid && fee.utxo.vout == request.funding.vout {
            return Err(SwapError::Chain(
                "spend fee UTXO overlaps the HTLC funding output".to_owned(),
            ));
        }

        let (_, fee_change_script) = self.wallet_address()?;
        let fee_change = fee
            .utxo
            .amount_sats
            .checked_sub(request.fee_sats)
            .ok_or_else(|| SwapError::Chain("fee UTXO is too small".to_owned()))?;
        let destination_script = p2wpkh_script(&request.keys.destination_public_key)?;
        let branch_key = SecretKey::from_slice(&request.keys.branch_private_key)
            .map_err(|_| SwapError::InvalidInput("invalid HTLC branch key".to_owned()))?;
        let args = LiquidSpendArgs {
            contract: request.contract.clone(),
            funding_txid: elements::Txid::from_str(&request.funding.txid)
                .map_err(|_| SwapError::InvalidInput("invalid funding txid".to_owned()))?,
            funding_vout: request.funding.vout,
            funding_amount_sats: request.funding.amount_sats,
            fee_input: fee.utxo.clone(),
            destination_script,
            fee_change_script,
            fee_change_amount_sats: fee_change,
            fee_sats: request.fee_sats,
            fee_asset_id: request.fee_asset_id,
            kind: request.kind,
        };
        let unsigned =
            build_liquid_htlc_spend(&args).map_err(|error| SwapError::Chain(error.to_string()))?;
        let signed = sign_liquid_htlc_spend(
            unsigned,
            &request.contract,
            &fee.utxo,
            &branch_key,
            &fee.private_key,
            request.funding.amount_sats,
            request.keys.preimage,
        )
        .map_err(|error| SwapError::Chain(error.to_string()))?;
        let outbox = FundingOutbox {
            kind: match request.kind {
                SpendKind::Claim => OutboxKind::Claim,
                SpendKind::Refund => OutboxKind::Refund,
            },
            txid: signed.txid,
            raw_hex: signed.raw_hex,
            funding_txid: Some(request.funding.txid.clone()),
            funding_vout: Some(request.funding.vout),
        };

        Ok(PreparedTransaction {
            outbox,
            funding_vout: None,
            asset_id: request.contract.asset_id,
            amount_sats: request.funding.amount_sats,
        })
    }

    fn observe_transaction_confirmations(&self, txid: &str) -> Result<u32, SwapError> {
        let transaction: Value = self.call("getrawtransaction", &[json!(txid), json!(true)])?;

        transaction
            .get("confirmations")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                SwapError::Chain("Elements transaction confirmations are missing".to_owned())
            })
    }

    fn mine_confirmation_block(&self) -> Result<(), SwapError> {
        self.mine_blocks(1)
    }
}

impl SwapChainAdapter for LiquidRpcAdapter {
    fn current_height(&self) -> Result<u64, SwapError> {
        self.ensure_regtest()
    }

    fn prepare_funding(
        &self,
        request: PrepareFundingRequest,
    ) -> Result<PreparedTransaction, SwapError> {
        self.make_funding(&request)
    }

    fn prepare_spend(
        &self,
        request: PrepareSpendRequest,
    ) -> Result<PreparedTransaction, SwapError> {
        self.make_spend(&request)
    }

    fn broadcast(&self, outbox: &FundingOutbox) -> Result<BroadcastEvidence, SwapError> {
        let broadcast = broadcast_liquid_transaction(&BroadcastLiquidArgs {
            rpc: &self.rpc,
            raw_hex: &outbox.raw_hex,
        })
        .map_err(|error| SwapError::Chain(error.to_string()))?;
        let confirmations = self
            .observe_transaction_confirmations(&broadcast.txid)
            .unwrap_or(0);

        Ok(BroadcastEvidence {
            txid: broadcast.txid,
            confirmations,
        })
    }

    fn confirm_funding(
        &self,
        _contract: &HtlcContract,
        _outbox: &FundingOutbox,
    ) -> Result<(), SwapError> {
        self.mine_confirmation_block()
    }

    fn confirm_spend(&self, _outbox: &FundingOutbox) -> Result<(), SwapError> {
        self.mine_confirmation_block()
    }

    fn observe_funding(
        &self,
        contract: &HtlcContract,
        txid: &str,
        vout: u32,
        amount_sats: u64,
    ) -> Result<FundingEvidence, SwapError> {
        inspect_liquid_htlc_output(&InspectLiquidHtlcArgs {
            rpc: &self.rpc,
            txid,
            vout,
            contract,
            expected_amount_sats: amount_sats,
            min_confirmations: MIN_CONFIRMATIONS,
            min_headroom_blocks: MIN_REFUND_HEADROOM,
        })
        .map_err(|error| SwapError::Chain(error.to_string()))
    }

    fn observe_claim(
        &self,
        contract: &HtlcContract,
        outbox: &FundingOutbox,
        expected_preimage: [u8; 32],
    ) -> Result<ClaimEvidence, SwapError> {
        let funding_txid = outbox
            .funding_txid
            .as_deref()
            .ok_or_else(|| SwapError::InvalidInput("claim outbox lacks funding txid".to_owned()))?;
        let funding_vout = outbox
            .funding_vout
            .ok_or_else(|| SwapError::InvalidInput("claim outbox lacks funding vout".to_owned()))?;
        let observation = observe_liquid_claim(&ObserveLiquidClaimArgs {
            rpc: &self.rpc,
            funding_txid,
            funding_vout,
            claim_txid: &outbox.txid,
            contract,
            min_confirmations: MIN_CONFIRMATIONS,
        })
        .map_err(|error| SwapError::Chain(error.to_string()))?;

        if observation.preimage.preimage != expected_preimage {
            return Err(SwapError::Chain(
                "Liquid claim preimage differs from the recovery record".to_owned(),
            ));
        }

        Ok(ClaimEvidence {
            txid: observation.claim_txid,
            funding_txid: funding_txid.to_owned(),
            funding_vout,
            confirmations: observation.preimage.confirmations,
            preimage_verified: true,
        })
    }

    fn observe_refund(&self, outbox: &FundingOutbox) -> Result<RefundEvidence, SwapError> {
        let funding_txid = outbox.funding_txid.as_deref().ok_or_else(|| {
            SwapError::InvalidInput("refund outbox lacks funding txid".to_owned())
        })?;
        let funding_vout = outbox.funding_vout.ok_or_else(|| {
            SwapError::InvalidInput("refund outbox lacks funding vout".to_owned())
        })?;
        let confirmations = self.observe_transaction_confirmations(&outbox.txid)?;

        if confirmations < MIN_CONFIRMATIONS {
            return Err(SwapError::Chain(
                "Elements refund is not sufficiently confirmed".to_owned(),
            ));
        }

        Ok(RefundEvidence {
            txid: outbox.txid.clone(),
            funding_txid: funding_txid.to_owned(),
            funding_vout,
            confirmations,
        })
    }
}

struct WalletUtxo {
    utxo: LiquidUtxo,
    private_key: SecretKey,
}

fn validate_funding_request(request: &PrepareFundingRequest) -> Result<(), SwapError> {
    if request.contract.chain != crate::chain::Chain::LiquidRegtest {
        return Err(SwapError::InvalidInput(
            "Elements adapter requires a Liquid regtest contract".to_owned(),
        ));
    }

    if request.contract.asset_id == AssetId::Bitcoin
        || request.fee_asset_id == AssetId::Bitcoin
        || request.amount_sats == 0
        || request.fee_sats == 0
    {
        return Err(SwapError::InvalidInput(
            "explicit asset, amount, and fee are required".to_owned(),
        ));
    }

    Ok(())
}

fn validate_spend_request(request: &PrepareSpendRequest) -> Result<(), SwapError> {
    validate_funding_request(&PrepareFundingRequest {
        contract: request.contract.clone(),
        amount_sats: request.funding.amount_sats,
        fee_sats: request.fee_sats,
        fee_asset_id: request.fee_asset_id,
    })?;

    if request.funding.asset_id != request.contract.asset_id {
        return Err(SwapError::InvalidInput(
            "funding asset differs from the HTLC contract".to_owned(),
        ));
    }

    if request.kind == SpendKind::Claim && request.keys.preimage.is_none() {
        return Err(SwapError::InvalidInput(
            "claim spend requires a preimage".to_owned(),
        ));
    }

    if request.kind == SpendKind::Refund && request.keys.preimage.is_some() {
        return Err(SwapError::InvalidInput(
            "refund spend must not carry a preimage".to_owned(),
        ));
    }

    Ok(())
}

fn parse_script(value: &str, label: &str) -> Result<ScriptBuf, SwapError> {
    let bytes =
        hex::decode(value).map_err(|_| SwapError::Chain(format!("{label} is not hexadecimal")))?;

    Ok(ScriptBuf::from_bytes(bytes))
}

fn parse_sats(value: &Value) -> Result<u64, SwapError> {
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    let mut parts = text.split('.');
    let whole = parts
        .next()
        .ok_or_else(|| SwapError::Chain("invalid Elements amount".to_owned()))?;
    let fraction = parts.next().unwrap_or("0");

    if parts.next().is_some() || whole.starts_with('-') || fraction.len() > 8 {
        return Err(SwapError::Chain("invalid Elements amount".to_owned()));
    }

    let whole = whole
        .parse::<u64>()
        .map_err(|_| SwapError::Chain("invalid Elements amount".to_owned()))?;
    let fraction_value = format!("{fraction:0<8}")
        .parse::<u64>()
        .map_err(|_| SwapError::Chain("invalid Elements amount".to_owned()))?;

    whole
        .checked_mul(100_000_000)
        .and_then(|value| value.checked_add(fraction_value))
        .ok_or_else(|| SwapError::Chain("Elements amount overflows satoshis".to_owned()))
}

fn decimal_sats(value: u64) -> String {
    format!("{}.{:08}", value / 100_000_000, value % 100_000_000)
}

fn has_commitment(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_str).is_some()
}

fn validate_wallet_script(script: &ScriptBuf, private_key: &SecretKey) -> Result<(), SwapError> {
    let secp = Secp256k1::new();
    let public_key = bitcoin::PublicKey::new(PublicKey::from_secret_key(&secp, private_key));
    let compressed = CompressedPublicKey::try_from(public_key)
        .map_err(|_| SwapError::Chain("wallet key must be compressed".to_owned()))?;
    let expected = ScriptBuf::new_p2wpkh(&compressed.wpubkey_hash());

    if expected != *script {
        return Err(SwapError::Chain(
            "wallet private key does not match the explicit UTXO script".to_owned(),
        ));
    }

    Ok(())
}

fn p2wpkh_script(public_key: &PublicKey) -> Result<ScriptBuf, SwapError> {
    let compressed = CompressedPublicKey::try_from(bitcoin::PublicKey::new(*public_key))
        .map_err(|_| SwapError::InvalidInput("destination key must be compressed".to_owned()))?;

    Ok(ScriptBuf::new_p2wpkh(&compressed.wpubkey_hash()))
}

fn same_outpoint(left: &LiquidUtxo, right: &LiquidUtxo) -> bool {
    left.txid == right.txid && left.vout == right.vout
}
