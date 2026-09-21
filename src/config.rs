use std::env;

use crate::core::error::{CoreError, CoreResult};
use crate::core::network::Network;

pub const DEFAULT_NETWORK: Network = Network::LiquidRegtest;
pub const DEFAULT_RUNTIME_DIR: &str = "runtime";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeConfig {
    pub network: Network,
    pub runtime_dir: String,
    pub solver_bind: String,
    pub client_store: String,
}

impl RuntimeConfig {
    pub fn from_env() -> CoreResult<Self> {
        let network_name = env_value("XMM_NETWORK", "POC_NETWORK", "regtest");

        if network_name != "regtest" && network_name != "liquid-regtest" {
            return Err(CoreError::UnsupportedNetwork(network_name));
        }

        let network = Network::LiquidRegtest;

        if !network.is_regtest() {
            return Err(CoreError::UnsupportedNetwork(network.to_string()));
        }

        let runtime_dir = env_value("XMM_RUNTIME_DIR", "POC_RUNTIME_DIR", DEFAULT_RUNTIME_DIR);
        let solver_bind = env_value("XMM_SOLVER_BIND", "POC_SOLVER_BIND", "127.0.0.1:37771");
        let client_store = env_value(
            "XMM_CLIENT_STORE",
            "POC_CLIENT_STORE",
            &format!("{runtime_dir}/client.sqlite"),
        );

        Ok(Self {
            network,
            runtime_dir,
            solver_bind,
            client_store,
        })
    }
}

fn env_value(primary: &str, legacy: &str, default: &str) -> String {
    env::var(primary)
        .or_else(|_| env::var(legacy))
        .unwrap_or_else(|_| default.to_owned())
}
