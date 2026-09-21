use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

use super::error::{CoreError, CoreResult};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CanonicalAmount(String);

impl CanonicalAmount {
    pub fn parse(value: &str) -> CoreResult<Self> {
        if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
            return Err(CoreError::InvalidAmount(value.to_owned()));
        }

        let parsed = value
            .parse::<u64>()
            .map_err(|_| CoreError::InvalidAmount(value.to_owned()))?;

        if parsed.to_string() != value {
            return Err(CoreError::InvalidAmount(value.to_owned()));
        }

        Ok(Self(value.to_owned()))
    }

    pub fn value(&self) -> CoreResult<u64> {
        self.0
            .parse::<u64>()
            .map_err(|_| CoreError::InvalidAmount(self.0.clone()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for CanonicalAmount {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for CanonicalAmount {
    type Error = CoreError;

    fn try_from(value: String) -> CoreResult<Self> {
        Self::parse(&value)
    }
}

impl From<CanonicalAmount> for String {
    fn from(value: CanonicalAmount) -> Self {
        value.0
    }
}
