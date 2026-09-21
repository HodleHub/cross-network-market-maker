use std::fmt::{Debug, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chain::{AssetId, Chain, HtlcContract, create_htlc_contract};

use super::error::SwapError;

/// Direction of the atomic swap.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SwapDirection {
    /// User funds Liquid and receives Lightning.
    DepixToLightning,
    /// User pays Lightning and receives Liquid.
    LightningToDepix,
}

/// Secret material needed to claim or refund a prepared HTLC.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct SwapKeyMaterial {
    /// 32-byte Lightning payment preimage.
    pub preimage: [u8; 32],
    /// SHA-256 commitment of `preimage`.
    pub hash_commitment: [u8; 32],
    /// Private claim branch key.
    pub claim_private_key: [u8; 32],
    /// Public claim branch key.
    pub claim_public_key: PublicKey,
    /// Private refund branch key.
    pub refund_private_key: [u8; 32],
    /// Public refund branch key.
    pub refund_public_key: PublicKey,
    /// Private destination key controlled by the client.
    pub destination_private_key: [u8; 32],
    /// Public destination key controlled by the client.
    pub destination_public_key: PublicKey,
}

impl Debug for SwapKeyMaterial {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SwapKeyMaterial")
            .field("preimage", &"<redacted>")
            .field("hash_commitment", &hex::encode(self.hash_commitment))
            .field("claim_private_key", &"<redacted>")
            .field("claim_public_key", &self.claim_public_key)
            .field("refund_private_key", &"<redacted>")
            .field("refund_public_key", &self.refund_public_key)
            .field("destination_private_key", &"<redacted>")
            .field("destination_public_key", &self.destination_public_key)
            .finish()
    }
}

/// Prepared quote-bound swap session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedSwapSession {
    /// Swap direction.
    pub direction: SwapDirection,
    /// Secret material persisted before funding.
    pub material: SwapKeyMaterial,
    /// HTLC asset on Liquid or Bitcoin.
    pub asset_id: AssetId,
    /// Fee asset used to construct the transaction.
    pub fee_asset_id: AssetId,
    /// Lightning amount in satoshis.
    pub amount_sats: u64,
    /// Chain asset amount in satoshis.
    pub asset_amount_sats: u64,
    /// Explicit fee in the fee asset.
    pub fee_sats: u64,
    /// Absolute refund lock height on the HTLC chain.
    pub refund_lock_height: u64,
    /// Optional quote identifier bound into the recovery record.
    pub quote_id: Option<String>,
}

/// Full deterministic recovery record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SwapRecoveryRecord {
    /// Recovery format version.
    pub version: u16,
    /// Quote-bound swap session.
    pub session: PreparedSwapSession,
    /// Reconstructible HTLC contract metadata.
    pub contract: HtlcContract,
}

#[derive(Deserialize, Serialize)]
struct RecoveryEnvelope {
    version: u16,
    nonce: String,
    ciphertext: String,
}

/// Inputs for creating or replaying one encrypted recovery record.
pub struct PrepareRecoveryRequest<'a> {
    /// Destination for the encrypted recovery record.
    pub path: &'a Path,
    /// Encryption key kept outside persisted public evidence.
    pub encryption_key: &'a [u8; 32],
    /// Swap direction.
    pub direction: SwapDirection,
    /// Chain carrying the HTLC asset.
    pub chain: Chain,
    /// Explicit HTLC asset.
    pub asset_id: AssetId,
    /// Explicit fee asset.
    pub fee_asset_id: AssetId,
    /// Lightning amount.
    pub amount_sats: u64,
    /// HTLC asset amount.
    pub asset_amount_sats: u64,
    /// Explicit chain fee.
    pub fee_sats: u64,
    /// Absolute refund height.
    pub refund_lock_height: u64,
    /// Optional quote identifier.
    pub quote_id: Option<String>,
    /// Existing material supplied by a caller resuming a prepared session.
    pub supplied_material: Option<SwapKeyMaterial>,
}

/// Generate fresh deterministic secret material before any chain or Lightning effect.
pub fn generate_swap_secrets() -> Result<SwapKeyMaterial, SwapError> {
    let secp = Secp256k1::new();
    let mut rng = rand::rngs::OsRng;
    let preimage = random_bytes(&mut rng);
    let claim_secret = SecretKey::new(&mut rng);
    let refund_secret = SecretKey::new(&mut rng);
    let destination_secret = SecretKey::new(&mut rng);
    let claim_private_key = claim_secret.secret_bytes();
    let refund_private_key = refund_secret.secret_bytes();
    let destination_private_key = destination_secret.secret_bytes();

    Ok(SwapKeyMaterial {
        preimage,
        hash_commitment: Sha256::digest(preimage).into(),
        claim_public_key: PublicKey::from_secret_key(&secp, &claim_secret),
        claim_private_key,
        refund_public_key: PublicKey::from_secret_key(&secp, &refund_secret),
        refund_private_key,
        destination_public_key: PublicKey::from_secret_key(&secp, &destination_secret),
        destination_private_key,
    })
}

/// Prepare or replay an encrypted recovery record before a funding broadcast.
pub fn prepare_or_load_recovery(
    request: PrepareRecoveryRequest<'_>,
) -> Result<(SwapRecoveryRecord, bool), SwapError> {
    if request.amount_sats == 0 || request.asset_amount_sats == 0 || request.refund_lock_height == 0
    {
        return Err(SwapError::InvalidInput(
            "swap amounts and refund height must be positive".to_owned(),
        ));
    }

    if let Some(record) = read_recovery(request.path, request.encryption_key)? {
        validate_recovery(&record, &request)?;

        return Ok((record, true));
    }

    let material = match request.supplied_material {
        Some(material) => material,
        None => generate_swap_secrets()?,
    };
    validate_material(&material)?;
    let contract = create_htlc_contract(
        request.chain,
        request.asset_id,
        material.hash_commitment,
        material.claim_public_key,
        material.refund_public_key,
        request.refund_lock_height,
    )
    .map_err(|error| SwapError::Chain(error.to_string()))?;
    let record = SwapRecoveryRecord {
        version: 1,
        session: PreparedSwapSession {
            direction: request.direction,
            material,
            asset_id: request.asset_id,
            fee_asset_id: request.fee_asset_id,
            amount_sats: request.amount_sats,
            asset_amount_sats: request.asset_amount_sats,
            fee_sats: request.fee_sats,
            refund_lock_height: request.refund_lock_height,
            quote_id: request.quote_id,
        },
        contract,
    };

    let replayed = write_recovery(request.path, request.encryption_key, &record)?;

    Ok((record, replayed))
}

/// Read and decrypt a recovery record without exposing its private material in logs.
pub fn read_recovery(
    path: &Path,
    encryption_key: &[u8; 32],
) -> Result<Option<SwapRecoveryRecord>, SwapError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(SwapError::Durability(error.to_string())),
    };
    let mut serialized = String::new();
    file.read_to_string(&mut serialized)
        .map_err(|error| SwapError::Durability(error.to_string()))?;
    let envelope: RecoveryEnvelope = serde_json::from_str(&serialized)
        .map_err(|error| SwapError::Durability(format!("invalid recovery envelope: {error}")))?;
    let nonce_bytes = hex::decode(envelope.nonce)
        .map_err(|_| SwapError::Durability("invalid recovery nonce".to_owned()))?;
    let nonce: [u8; 12] = nonce_bytes
        .try_into()
        .map_err(|_| SwapError::Durability("invalid recovery nonce length".to_owned()))?;
    let ciphertext = STANDARD
        .decode(envelope.ciphertext)
        .map_err(|_| SwapError::Durability("invalid recovery ciphertext".to_owned()))?;
    let cipher = Aes256Gcm::new_from_slice(encryption_key)
        .map_err(|error| SwapError::Durability(error.to_string()))?;
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map_err(|_| SwapError::Durability("recovery decryption failed".to_owned()))?;
    let record = serde_json::from_slice(&plaintext)
        .map_err(|error| SwapError::Durability(format!("invalid recovery record: {error}")))?;

    Ok(Some(record))
}

fn write_recovery(
    path: &Path,
    encryption_key: &[u8; 32],
    record: &SwapRecoveryRecord,
) -> Result<bool, SwapError> {
    ensure_parent(path)?;
    let plaintext =
        serde_json::to_vec(record).map_err(|error| SwapError::Durability(error.to_string()))?;
    let cipher = Aes256Gcm::new_from_slice(encryption_key)
        .map_err(|error| SwapError::Durability(error.to_string()))?;
    let mut nonce = [0_u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext.as_ref())
        .map_err(|_| SwapError::Durability("recovery encryption failed".to_owned()))?;
    let envelope = RecoveryEnvelope {
        version: 1,
        nonce: hex::encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
    };
    let serialized =
        serde_json::to_vec(&envelope).map_err(|error| SwapError::Durability(error.to_string()))?;

    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(&serialized)
                .and_then(|_| file.write_all(b"\n"))
                .and_then(|_| file.sync_all())
                .map_err(|error| SwapError::Durability(error.to_string()))?;
            set_private(path)?;
            sync_parent(path)?;

            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_recovery(path, encryption_key)?
                .ok_or_else(|| SwapError::Durability("recovery disappeared".to_owned()))?;

            if existing == *record {
                return Ok(true);
            }

            Err(SwapError::RecoveryMismatch(
                "recovery record was mutated".to_owned(),
            ))
        }
        Err(error) => Err(SwapError::Durability(error.to_string())),
    }
}

fn validate_recovery(
    record: &SwapRecoveryRecord,
    request: &PrepareRecoveryRequest<'_>,
) -> Result<(), SwapError> {
    let session = &record.session;

    if record.version != 1
        || session.direction != request.direction
        || session.asset_id != request.asset_id
        || session.fee_asset_id != request.fee_asset_id
        || session.amount_sats != request.amount_sats
        || session.asset_amount_sats != request.asset_amount_sats
        || session.fee_sats != request.fee_sats
        || record.contract.chain != request.chain
        || record.contract.asset_id != request.asset_id
    {
        return Err(SwapError::RecoveryMismatch(
            "recovery session terms differ".to_owned(),
        ));
    }

    validate_material(&session.material)?;
    let expected = create_htlc_contract(
        request.chain,
        request.asset_id,
        session.material.hash_commitment,
        session.material.claim_public_key,
        session.material.refund_public_key,
        session.refund_lock_height,
    )
    .map_err(|error| SwapError::Chain(error.to_string()))?;

    if expected != record.contract {
        return Err(SwapError::RecoveryMismatch(
            "contract metadata differs".to_owned(),
        ));
    }

    Ok(())
}

fn validate_material(material: &SwapKeyMaterial) -> Result<(), SwapError> {
    if Sha256::digest(material.preimage).as_slice() != material.hash_commitment {
        return Err(SwapError::RecoveryMismatch(
            "preimage hash differs".to_owned(),
        ));
    }

    let secp = Secp256k1::new();
    validate_key_pair(&secp, material.claim_private_key, material.claim_public_key)?;
    validate_key_pair(
        &secp,
        material.refund_private_key,
        material.refund_public_key,
    )?;
    validate_key_pair(
        &secp,
        material.destination_private_key,
        material.destination_public_key,
    )
}

fn validate_key_pair(
    secp: &Secp256k1<secp256k1::All>,
    private_key: [u8; 32],
    public_key: PublicKey,
) -> Result<(), SwapError> {
    let secret = SecretKey::from_slice(&private_key)
        .map_err(|_| SwapError::RecoveryMismatch("invalid private key".to_owned()))?;

    if PublicKey::from_secret_key(secp, &secret) != public_key {
        return Err(SwapError::RecoveryMismatch(
            "private/public key mismatch".to_owned(),
        ));
    }

    Ok(())
}

fn random_bytes(rng: &mut rand::rngs::OsRng) -> [u8; 32] {
    let mut bytes = [0_u8; 32];
    rng.fill_bytes(&mut bytes);

    bytes
}

fn ensure_parent(path: &Path) -> Result<(), SwapError> {
    if let Some(parent) = path.parent() {
        #[cfg(unix)]
        {
            let result = fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent);

            if let Err(error) = result
                && (error.kind() != std::io::ErrorKind::AlreadyExists || !parent.is_dir())
            {
                return Err(SwapError::Durability(error.to_string()));
            }
        }

        #[cfg(not(unix))]
        fs::create_dir_all(parent).map_err(|error| SwapError::Durability(error.to_string()))?;
    }

    Ok(())
}

fn sync_parent(path: &Path) -> Result<(), SwapError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| SwapError::Durability(error.to_string()))
}

fn set_private(path: &Path) -> Result<(), SwapError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path)
            .map_err(|error| SwapError::Durability(error.to_string()))?
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(path, permissions)
            .map_err(|error| SwapError::Durability(error.to_string()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        PrepareRecoveryRequest, SwapDirection, generate_swap_secrets, prepare_or_load_recovery,
        read_recovery,
    };
    use crate::chain::{AssetId, Chain};

    #[test]
    fn recovery_is_encrypted_and_replay_is_immutable() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("forward-recovery.json");
        let key = [7_u8; 32];
        let material = generate_swap_secrets().expect("generated test material");
        let asset_id = AssetId::Explicit([3_u8; 32]);
        let fee_asset_id = AssetId::Explicit([4_u8; 32]);
        let (record, replayed) = prepare_or_load_recovery(PrepareRecoveryRequest {
            path: &path,
            encryption_key: &key,
            direction: SwapDirection::DepixToLightning,
            chain: Chain::LiquidRegtest,
            asset_id,
            fee_asset_id,
            amount_sats: 20_000,
            asset_amount_sats: 100_000,
            fee_sats: 1_000,
            refund_lock_height: 1_000,
            quote_id: Some("quote-1".to_owned()),
            supplied_material: Some(material),
        })
        .expect("recovery prepared");

        assert!(!replayed);
        let serialized = fs::read_to_string(&path).expect("recovery file");
        assert!(!serialized.contains(&hex::encode(record.session.material.preimage)));

        let (same_record, replayed) = prepare_or_load_recovery(PrepareRecoveryRequest {
            path: &path,
            encryption_key: &key,
            direction: SwapDirection::DepixToLightning,
            chain: Chain::LiquidRegtest,
            asset_id,
            fee_asset_id,
            amount_sats: 20_000,
            asset_amount_sats: 100_000,
            fee_sats: 1_000,
            refund_lock_height: 1_000,
            quote_id: Some("quote-1".to_owned()),
            supplied_material: None,
        })
        .expect("recovery replay");

        assert!(replayed);
        assert_eq!(same_record, record);
        assert!(read_recovery(&path, &key).expect("recovery read").is_some());
    }
}
