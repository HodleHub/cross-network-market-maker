use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use secp256k1::{Message, PublicKey, Secp256k1, SecretKey, ecdsa::Signature};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::amount::CanonicalAmount;
use super::endpoint::EndpointId;
use super::error::{CoreError, CoreResult};
use super::hash::HashCommitment;
use super::network::Network;
use super::routes::RouteRegistry;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeadlineContext {
    pub source_height: u64,
    pub destination_height: u64,
    pub source_refund_height: u64,
    pub destination_refund_height: u64,
    pub lightning_expiry_height: Option<u64>,
    pub source_refund_at: u64,
    pub destination_refund_at: u64,
    pub lightning_expiry_at: Option<u64>,
    pub margin_seconds: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Intent {
    pub version: u16,
    pub intent_id: String,
    pub source: EndpointId,
    pub destination: EndpointId,
    pub amount_in: CanonicalAmount,
    pub min_amount_out: CanonicalAmount,
    pub fee_limit_lbtc: CanonicalAmount,
    pub hash_commitment: HashCommitment,
    pub client_claim_pubkey: String,
    pub client_refund_pubkey: String,
    pub source_destination: String,
    pub destination_destination: String,
    pub refund_destination: String,
    pub deadline_context: DeadlineContext,
    pub created_at: u64,
    pub expires_at: u64,
    pub nonce: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    pub version: u16,
    pub quote_id: String,
    pub intent_id: String,
    pub source: EndpointId,
    pub destination: EndpointId,
    pub amount_in: CanonicalAmount,
    pub min_amount_out: CanonicalAmount,
    pub amount_out: CanonicalAmount,
    pub fee_lbtc: CanonicalAmount,
    pub fee_limit_lbtc: CanonicalAmount,
    pub hash_commitment: HashCommitment,
    pub client_claim_pubkey: String,
    pub client_refund_pubkey: String,
    pub source_destination: String,
    pub destination_destination: String,
    pub refund_destination: String,
    pub source_refund_height: u64,
    pub destination_refund_height: u64,
    pub lightning_expiry_height: Option<u64>,
    pub deadline_context: DeadlineContext,
    pub expires_at: u64,
    pub nonce: String,
    pub solver_key_id: String,
    pub adapter_id: String,
    pub payment_request: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedQuote {
    pub quote: Quote,
    pub signature_hex: String,
}

#[derive(Clone, Debug, Default)]
pub struct PinnedSolverKeys {
    keys: BTreeMap<String, String>,
}

impl PinnedSolverKeys {
    pub fn insert(&mut self, key_id: impl Into<String>, public_key_hex: impl Into<String>) {
        self.keys.insert(key_id.into(), public_key_hex.into());
    }

    pub fn get(&self, key_id: &str) -> Option<&str> {
        self.keys.get(key_id).map(String::as_str)
    }
}

pub fn now_unix_seconds() -> CoreResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| CoreError::Serialization(error.to_string()))
}

pub fn sign_quote(quote: &Quote, secret_key_hex: &str) -> CoreResult<SignedQuote> {
    let secret_key_bytes = hex::decode(secret_key_hex)
        .map_err(|_| CoreError::Signature("secret key is not hex".to_owned()))?;
    let secret_key = SecretKey::from_slice(&secret_key_bytes)
        .map_err(|error| CoreError::Signature(error.to_string()))?;
    let message = Message::from_digest(quote_digest(quote));
    let signature = Secp256k1::new().sign_ecdsa(&message, &secret_key);

    Ok(SignedQuote {
        quote: quote.clone(),
        signature_hex: hex::encode(signature.serialize_compact()),
    })
}

pub fn verify_quote(
    intent: &Intent,
    signed_quote: &SignedQuote,
    pinned_keys: &PinnedSolverKeys,
    registry: &RouteRegistry,
    context: &DeadlineContext,
    now: u64,
) -> CoreResult<()> {
    validate_quote_terms(intent, &signed_quote.quote, registry, context, now)?;
    let public_key_hex = pinned_keys
        .get(&signed_quote.quote.solver_key_id)
        .ok_or_else(|| CoreError::Signature("solver key is not pinned".to_owned()))?;
    let public_key_bytes = hex::decode(public_key_hex)
        .map_err(|_| CoreError::Signature("pinned solver key is not hex".to_owned()))?;
    let public_key = PublicKey::from_slice(&public_key_bytes)
        .map_err(|error| CoreError::Signature(error.to_string()))?;
    let signature_bytes = hex::decode(&signed_quote.signature_hex)
        .map_err(|_| CoreError::Signature("quote signature is not hex".to_owned()))?;
    let signature = Signature::from_compact(&signature_bytes)
        .map_err(|error| CoreError::Signature(error.to_string()))?;
    let message = Message::from_digest(quote_digest(&signed_quote.quote));

    Secp256k1::new()
        .verify_ecdsa(&message, &signature, &public_key)
        .map_err(|error| CoreError::Signature(error.to_string()))
}

pub fn quote_digest(quote: &Quote) -> [u8; 32] {
    let mut bytes = Vec::new();
    append_field(&mut bytes, "xmm-quote-v1");
    append_field(&mut bytes, &quote.version.to_string());
    append_field(&mut bytes, &quote.quote_id);
    append_field(&mut bytes, &quote.intent_id);
    append_field(&mut bytes, &quote.source.canonical_id());
    append_field(&mut bytes, &quote.destination.canonical_id());
    append_field(&mut bytes, quote.source.asset_hash.as_deref().unwrap_or(""));
    append_field(
        &mut bytes,
        quote.destination.asset_hash.as_deref().unwrap_or(""),
    );
    append_field(&mut bytes, quote.amount_in.as_str());
    append_field(&mut bytes, quote.min_amount_out.as_str());
    append_field(&mut bytes, quote.amount_out.as_str());
    append_field(&mut bytes, quote.fee_lbtc.as_str());
    append_field(&mut bytes, quote.fee_limit_lbtc.as_str());
    append_field(&mut bytes, &quote.hash_commitment.as_hex());
    append_field(&mut bytes, &quote.client_claim_pubkey);
    append_field(&mut bytes, &quote.client_refund_pubkey);
    append_field(&mut bytes, &quote.source_destination);
    append_field(&mut bytes, &quote.destination_destination);
    append_field(&mut bytes, &quote.refund_destination);
    append_field(&mut bytes, &quote.source_refund_height.to_string());
    append_field(&mut bytes, &quote.destination_refund_height.to_string());
    append_field(
        &mut bytes,
        &quote
            .lightning_expiry_height
            .map(|value| value.to_string())
            .unwrap_or_default(),
    );
    append_field(
        &mut bytes,
        &quote.deadline_context.source_height.to_string(),
    );
    append_field(
        &mut bytes,
        &quote.deadline_context.destination_height.to_string(),
    );
    append_field(
        &mut bytes,
        &quote.deadline_context.source_refund_at.to_string(),
    );
    append_field(
        &mut bytes,
        &quote.deadline_context.destination_refund_at.to_string(),
    );
    append_field(
        &mut bytes,
        &quote
            .deadline_context
            .lightning_expiry_at
            .map(|value| value.to_string())
            .unwrap_or_default(),
    );
    append_field(
        &mut bytes,
        &quote.deadline_context.margin_seconds.to_string(),
    );
    append_field(&mut bytes, &quote.expires_at.to_string());
    append_field(&mut bytes, &quote.nonce);
    append_field(&mut bytes, &quote.solver_key_id);
    append_field(&mut bytes, &quote.adapter_id);
    append_field(&mut bytes, quote.payment_request.as_deref().unwrap_or(""));

    Sha256::digest(bytes).into()
}

fn validate_quote_terms(
    intent: &Intent,
    quote: &Quote,
    registry: &RouteRegistry,
    context: &DeadlineContext,
    now: u64,
) -> CoreResult<()> {
    if intent.version != 1 || quote.version != 1 || intent.version != quote.version {
        return Err(CoreError::QuoteRejected(
            "unsupported or mismatched protocol version".to_owned(),
        ));
    }

    intent.source.validate()?;
    intent.destination.validate()?;
    quote.source.validate()?;
    quote.destination.validate()?;

    if intent.source.asset_id == "TEST-DEPIX" && intent.source.asset_hash.is_none()
        || intent.destination.asset_id == "TEST-DEPIX" && intent.destination.asset_hash.is_none()
    {
        return Err(CoreError::QuoteRejected(
            "TEST-DEPIX asset hash is required for settlement".to_owned(),
        ));
    }

    if quote.intent_id != intent.intent_id
        || quote.source != intent.source
        || quote.destination != intent.destination
        || quote.amount_in != intent.amount_in
        || quote.min_amount_out != intent.min_amount_out
        || quote.fee_limit_lbtc != intent.fee_limit_lbtc
        || quote.hash_commitment != intent.hash_commitment
        || quote.client_claim_pubkey != intent.client_claim_pubkey
        || quote.client_refund_pubkey != intent.client_refund_pubkey
        || quote.source_destination != intent.source_destination
        || quote.destination_destination != intent.destination_destination
        || quote.refund_destination != intent.refund_destination
        || quote.deadline_context != intent.deadline_context
        || quote.nonce != intent.nonce
    {
        return Err(CoreError::QuoteRejected(
            "quote does not bind the signed intent".to_owned(),
        ));
    }

    if !registry.is_enabled(&intent.source, &intent.destination) {
        return Err(CoreError::UnsupportedRoute {
            source_endpoint: intent.source.canonical_id(),
            destination_endpoint: intent.destination.canonical_id(),
        });
    }

    let route = registry
        .route(&intent.source, &intent.destination)
        .ok_or_else(|| CoreError::QuoteRejected("route is absent from registry".to_owned()))?;

    if quote.adapter_id != route.adapter_id {
        return Err(CoreError::QuoteRejected(
            "quote adapter is not the qualified route adapter".to_owned(),
        ));
    }

    if quote.amount_out.value()? < intent.min_amount_out.value()?
        || quote.fee_lbtc.value()? > intent.fee_limit_lbtc.value()?
        || quote.expires_at > intent.expires_at
        || quote.expires_at <= now
    {
        return Err(CoreError::QuoteRejected(
            "amount, fee, or expiry policy rejected".to_owned(),
        ));
    }

    if quote.deadline_context != *context {
        return Err(CoreError::DeadlineRejected(
            "quote deadline context does not match trusted observations".to_owned(),
        ));
    }

    validate_deadline_order(quote, context, now)
}

fn validate_deadline_order(quote: &Quote, context: &DeadlineContext, now: u64) -> CoreResult<()> {
    if context.source_refund_at <= now || context.destination_refund_at <= now {
        return Err(CoreError::DeadlineRejected(
            "refund deadline is already expired".to_owned(),
        ));
    }

    if let Some(lightning_expiry_at) = context.lightning_expiry_at {
        let source_is_lightning = quote.source.network == Network::LightningRegtest;
        let source_deadline = if source_is_lightning {
            lightning_expiry_at
        } else {
            context.source_refund_at
        };
        let destination_deadline = if source_is_lightning {
            context.destination_refund_at
        } else {
            lightning_expiry_at
        };

        if source_deadline <= destination_deadline.saturating_add(context.margin_seconds) {
            return Err(CoreError::DeadlineRejected(
                "source refund/expiry is not later than destination by the safety margin"
                    .to_owned(),
            ));
        }
    }

    if quote.source_refund_height != context.source_refund_height
        || quote.destination_refund_height != context.destination_refund_height
        || quote.lightning_expiry_height != context.lightning_expiry_height
    {
        return Err(CoreError::DeadlineRejected(
            "quote deadline height does not match trusted context".to_owned(),
        ));
    }

    Ok(())
}

fn append_field(output: &mut Vec<u8>, value: &str) {
    let length = u64::try_from(value.len()).unwrap_or(u64::MAX);
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value.as_bytes());
}
