use std::fmt::{Debug, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use serde::{Deserialize, Serialize};

use super::error::SwapError;

/// Durable transaction intent kind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboxKind {
    /// HTLC funding transaction.
    Funding,
    /// Claim transaction revealing the payment preimage.
    Claim,
    /// Absolute-height refund transaction.
    Refund,
}

/// Signed transaction persisted before a broadcast attempt.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct FundingOutbox {
    /// Funding, claim, or refund role.
    pub kind: OutboxKind,
    /// Deterministic transaction identifier.
    pub txid: String,
    /// Signed transaction bytes; never printed in public evidence.
    pub raw_hex: String,
    /// Original funding transaction when this is a claim or refund.
    pub funding_txid: Option<String>,
    /// Original funding output when this is a claim or refund.
    pub funding_vout: Option<u32>,
}

impl Debug for FundingOutbox {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FundingOutbox")
            .field("kind", &self.kind)
            .field("txid", &self.txid)
            .field("raw_hex", &"<redacted>")
            .field("funding_txid", &self.funding_txid)
            .field("funding_vout", &self.funding_vout)
            .finish()
    }
}

/// Read a durable outbox, treating an absent path as not yet prepared.
pub fn read_outbox(path: &Path) -> Result<Option<FundingOutbox>, SwapError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(SwapError::Durability(error.to_string())),
    };
    let mut serialized = String::new();
    file.read_to_string(&mut serialized)
        .map_err(|error| SwapError::Durability(error.to_string()))?;

    serde_json::from_str(&serialized)
        .map(Some)
        .map_err(|error| SwapError::Durability(format!("invalid outbox JSON: {error}")))
}

/// Persist an outbox with exclusive creation and file synchronization.
pub fn write_outbox(path: &Path, record: &FundingOutbox) -> Result<bool, SwapError> {
    ensure_parent(path)?;
    let serialized =
        serde_json::to_vec(record).map_err(|error| SwapError::Durability(error.to_string()))?;

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
            let existing = read_outbox(path)?
                .ok_or_else(|| SwapError::Durability("outbox disappeared".to_owned()))?;

            if existing == *record {
                return Ok(true);
            }

            Err(SwapError::RecoveryMismatch(
                "funding outbox was mutated".to_owned(),
            ))
        }
        Err(error) => Err(SwapError::Durability(error.to_string())),
    }
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
