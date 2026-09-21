use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::error::{CoreError, CoreResult};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Network {
    LiquidRegtest,
    BitcoinRegtest,
    LightningRegtest,
    ArkRegtest,
}

impl Network {
    pub const fn is_regtest(self) -> bool {
        matches!(
            self,
            Self::LiquidRegtest | Self::BitcoinRegtest | Self::LightningRegtest | Self::ArkRegtest
        )
    }

    pub const fn block_seconds(self) -> Option<u64> {
        match self {
            Self::BitcoinRegtest => Some(600),
            Self::LiquidRegtest => Some(60),
            Self::LightningRegtest | Self::ArkRegtest => None,
        }
    }
}

impl Display for Network {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::LiquidRegtest => "liquid-regtest",
            Self::BitcoinRegtest => "bitcoin-regtest",
            Self::LightningRegtest => "lightning-regtest",
            Self::ArkRegtest => "ark-regtest",
        };

        formatter.write_str(value)
    }
}

impl FromStr for Network {
    type Err = CoreError;

    fn from_str(value: &str) -> CoreResult<Self> {
        match value {
            "liquid-regtest" => Ok(Self::LiquidRegtest),
            "bitcoin-regtest" => Ok(Self::BitcoinRegtest),
            "lightning-regtest" => Ok(Self::LightningRegtest),
            "ark-regtest" => Ok(Self::ArkRegtest),
            other => Err(CoreError::InvalidNetwork(other.to_owned())),
        }
    }
}
