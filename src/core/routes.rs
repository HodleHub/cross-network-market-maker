use serde::{Deserialize, Serialize};

use super::endpoint::EndpointId;
use super::error::CoreResult;
use super::network::Network;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectedRoute {
    pub source: EndpointId,
    pub destination: EndpointId,
    pub enabled: bool,
    pub adapter_id: String,
}

#[derive(Clone, Debug)]
pub struct RouteRegistry {
    routes: Vec<DirectedRoute>,
}

impl RouteRegistry {
    pub fn regtest() -> CoreResult<Self> {
        Self::build(None)
    }

    pub fn qualified(test_depix_asset_hash: &str) -> CoreResult<Self> {
        if test_depix_asset_hash.is_empty() {
            return Err(super::error::CoreError::InvalidEndpoint(
                "TEST-DEPIX asset hash is required".to_owned(),
            ));
        }

        Self::build(Some(test_depix_asset_hash.to_owned()))
    }

    pub fn route(&self, source: &EndpointId, destination: &EndpointId) -> Option<&DirectedRoute> {
        self.routes
            .iter()
            .find(|route| route.source == *source && route.destination == *destination)
    }

    fn build(test_depix_asset_hash: Option<String>) -> CoreResult<Self> {
        let endpoints = [
            EndpointId::new("TEST-DEPIX", Network::LiquidRegtest, test_depix_asset_hash)?,
            EndpointId::new("BTC", Network::LightningRegtest, None)?,
            EndpointId::new("BTC", Network::ArkRegtest, None)?,
            EndpointId::new("BTC", Network::BitcoinRegtest, None)?,
            EndpointId::new("LBTC", Network::LiquidRegtest, None)?,
        ];
        let routes = endpoints
            .iter()
            .flat_map(|source| {
                endpoints
                    .iter()
                    .map(move |destination| (source, destination))
            })
            .filter(|(source, destination)| source != destination)
            .map(|(source, destination)| DirectedRoute {
                source: source.clone(),
                destination: destination.clone(),
                enabled: is_initial_route(source, destination),
                adapter_id: adapter_id(source, destination),
            })
            .collect::<Vec<_>>();

        Ok(Self { routes })
    }

    pub fn all(&self) -> &[DirectedRoute] {
        &self.routes
    }

    pub fn is_enabled(&self, source: &EndpointId, destination: &EndpointId) -> bool {
        self.route(source, destination)
            .is_some_and(|route| route.enabled)
    }
}

fn is_initial_route(source: &EndpointId, destination: &EndpointId) -> bool {
    let endpoint_pair = matches!(
        (source.network, destination.network),
        (Network::LiquidRegtest, Network::LightningRegtest)
            | (Network::LightningRegtest, Network::LiquidRegtest)
    );
    let asset_pair = (source.asset_id == "TEST-DEPIX" && destination.asset_id == "BTC")
        || (source.asset_id == "BTC" && destination.asset_id == "TEST-DEPIX");

    endpoint_pair
        && asset_pair
        && (source.asset_id != "TEST-DEPIX" || source.asset_hash.is_some())
        && (destination.asset_id != "TEST-DEPIX" || destination.asset_hash.is_some())
}

fn adapter_id(source: &EndpointId, destination: &EndpointId) -> String {
    if source.network == Network::ArkRegtest || destination.network == Network::ArkRegtest {
        return "unqualified-ark".to_owned();
    }

    if source.network == Network::LightningRegtest
        || destination.network == Network::LightningRegtest
    {
        return "lightning-hold".to_owned();
    }

    "chain-htlc".to_owned()
}
