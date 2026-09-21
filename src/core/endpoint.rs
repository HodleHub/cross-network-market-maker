use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

use super::error::{CoreError, CoreResult};
use super::network::Network;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct EndpointId {
    pub asset_id: String,
    pub network: Network,
    pub asset_hash: Option<String>,
}

impl EndpointId {
    pub fn new(
        asset_id: impl Into<String>,
        network: Network,
        asset_hash: Option<String>,
    ) -> CoreResult<Self> {
        let endpoint = Self {
            asset_id: asset_id.into(),
            network,
            asset_hash,
        };

        endpoint.validate()?;

        Ok(endpoint)
    }

    pub fn canonical_id(&self) -> String {
        format!("{}@{}", self.asset_id, self.network)
    }

    pub fn validate(&self) -> CoreResult<()> {
        if self.asset_id.is_empty()
            || self.asset_id.chars().any(|character| {
                !(character.is_ascii_uppercase() || character.is_ascii_digit() || character == '-')
            })
        {
            return Err(CoreError::InvalidEndpoint(self.canonical_id()));
        }

        if !self.network.is_regtest() {
            return Err(CoreError::UnsupportedNetwork(self.network.to_string()));
        }

        if self.asset_id == "TEST-DEPIX" && self.network != Network::LiquidRegtest {
            return Err(CoreError::InvalidEndpoint(self.canonical_id()));
        }

        if self.asset_id == "LBTC" && self.network != Network::LiquidRegtest {
            return Err(CoreError::InvalidEndpoint(self.canonical_id()));
        }

        if self.asset_id == "BTC"
            && !matches!(
                self.network,
                Network::BitcoinRegtest | Network::LightningRegtest | Network::ArkRegtest
            )
        {
            return Err(CoreError::InvalidEndpoint(self.canonical_id()));
        }

        if let Some(asset_hash) = &self.asset_hash {
            validate_asset_hash(asset_hash)?;
        }

        Ok(())
    }
}

impl Display for EndpointId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.canonical_id())
    }
}

fn validate_asset_hash(value: &str) -> CoreResult<()> {
    if value.len() != 64 || value.to_lowercase() != value || hex::decode(value).is_err() {
        return Err(CoreError::InvalidEndpoint(format!("asset hash {value}")));
    }

    Ok(())
}
