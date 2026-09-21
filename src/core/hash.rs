use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::{CoreError, CoreResult};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HashCommitment([u8; 32]);

impl HashCommitment {
    pub fn parse(value: &str) -> CoreResult<Self> {
        let bytes = hex::decode(value).map_err(|_| CoreError::InvalidHash(value.to_owned()))?;

        if bytes.len() != 32 || value.len() != 64 || value.to_lowercase() != value {
            return Err(CoreError::InvalidHash(value.to_owned()));
        }

        let mut output = [0_u8; 32];
        output.copy_from_slice(&bytes);

        Ok(Self(output))
    }

    pub fn from_preimage(preimage: &[u8; 32]) -> Self {
        let digest = Sha256::digest(preimage);
        let mut output = [0_u8; 32];
        output.copy_from_slice(&digest);

        Self(output)
    }

    pub fn bytes(&self) -> [u8; 32] {
        self.0
    }

    pub fn as_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl TryFrom<String> for HashCommitment {
    type Error = CoreError;

    fn try_from(value: String) -> CoreResult<Self> {
        Self::parse(&value)
    }
}

impl From<HashCommitment> for String {
    fn from(value: HashCommitment) -> Self {
        value.as_hex()
    }
}

pub fn hash_preimage(preimage_hex: &str) -> CoreResult<HashCommitment> {
    let bytes =
        hex::decode(preimage_hex).map_err(|_| CoreError::InvalidHash(preimage_hex.to_owned()))?;

    if bytes.len() != 32 {
        return Err(CoreError::InvalidHash(preimage_hex.to_owned()));
    }

    let mut preimage = [0_u8; 32];
    preimage.copy_from_slice(&bytes);

    Ok(HashCommitment::from_preimage(&preimage))
}
