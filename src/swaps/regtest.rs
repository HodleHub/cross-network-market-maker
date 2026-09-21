//! Private local fixture loading for ignored real-node swap tests.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rand::RngCore;
use serde::Deserialize;

use crate::lightning::{LightningNetwork, LndRestClient, LndRestConfig};
use crate::rpc::RpcClient;

use super::SwapError;
use super::{LiquidRpcAdapter, SwapEncryptionKey, SwapPaths};

const DEFAULT_RUNTIME_DIR: &str = "runtime";
const DEFAULT_RPC_USER: &str = "cross_network_market_maker";
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(20);

/// Public fixture fields needed by the local Rust test harness.
#[derive(Clone, Debug, Deserialize)]
pub struct RegtestFixture {
    /// Elements endpoint and wallet identity.
    pub elements: ElementsFixture,
    /// Alice LND endpoint.
    pub lightning: LightningFixture,
}

/// Elements public endpoint configuration.
#[derive(Clone, Debug, Deserialize)]
pub struct ElementsFixture {
    /// Loopback Elements RPC endpoint.
    #[serde(rename = "rpcUrl")]
    pub rpc_url: String,
    /// Wallet name.
    pub wallet: String,
    /// Policy asset identifier.
    #[serde(rename = "policyAssetId")]
    pub policy_asset_id: String,
    /// TEST-DEPIX asset identifier.
    #[serde(rename = "testDepixAssetId")]
    pub test_depix_asset_id: String,
}

/// One LND public REST endpoint.
#[derive(Clone, Debug, Deserialize)]
pub struct LightningFixture {
    /// Alice REST configuration.
    pub alice: LndNodeFixture,
    /// Bob REST configuration.
    pub bob: LndNodeFixture,
}

/// LND loopback endpoint and public identity.
#[derive(Clone, Debug, Deserialize)]
pub struct LndNodeFixture {
    /// HTTPS REST endpoint.
    #[serde(rename = "restUrl")]
    pub rest_url: String,
}

/// Live clients for the dedicated fresh regtest stack.
pub struct RegtestSwapHarness {
    /// Public fixture data.
    pub fixture: RegtestFixture,
    /// Explicit Elements wallet adapter.
    pub elements: LiquidRpcAdapter,
    /// Alice LND REST client.
    pub alice: LndRestClient,
    /// Bob LND REST client.
    pub bob: LndRestClient,
    runtime_dir: PathBuf,
}

impl RegtestSwapHarness {
    /// Loads only the dedicated runtime fixture and private local credentials.
    pub fn load() -> Result<Self, SwapError> {
        let runtime_dir = runtime_dir();
        let fixture_path = runtime_dir.join("public-fixture.json");
        let fixture = read_json::<RegtestFixture>(&fixture_path)?;
        let rpc_password = required_env("XMM_ELEMENTS_RPC_PASSWORD")?;
        let rpc_user =
            std::env::var("XMM_ELEMENTS_RPC_USER").unwrap_or_else(|_| DEFAULT_RPC_USER.to_owned());
        let rpc = RpcClient::new(
            fixture.elements.rpc_url.clone(),
            rpc_user,
            rpc_password,
            DEFAULT_RPC_TIMEOUT,
        )
        .map_err(|error| SwapError::Chain(error.to_string()))?;
        let policy_asset_id = crate::chain::AssetId::from_hex(&fixture.elements.policy_asset_id)
            .map_err(|error| SwapError::Chain(error.to_string()))?;
        let elements =
            LiquidRpcAdapter::new(rpc, fixture.elements.wallet.clone(), policy_asset_id)?;
        let alice = load_lnd(&runtime_dir, "lnd-alice", &fixture.lightning.alice.rest_url)?;
        let bob = load_lnd(&runtime_dir, "lnd-bob", &fixture.lightning.bob.rest_url)?;

        Ok(Self {
            fixture,
            elements,
            alice,
            bob,
            runtime_dir,
        })
    }

    /// Prepares explicit test coins in one wallet transaction and confirms them.
    pub fn prepare_liquidity(&self, amount_sats: u64) -> Result<String, SwapError> {
        let asset_id = crate::chain::AssetId::from_hex(&self.fixture.elements.test_depix_asset_id)
            .map_err(|error| SwapError::Chain(error.to_string()))?;
        let fee_asset_id = crate::chain::AssetId::from_hex(&self.fixture.elements.policy_asset_id)
            .map_err(|error| SwapError::Chain(error.to_string()))?;
        let txid = self
            .elements
            .prepare_explicit_liquidity(asset_id, fee_asset_id, amount_sats)?;
        self.elements.mine_one_block()?;

        Ok(txid)
    }

    /// Returns a private encryption key persisted for one live test process.
    pub fn encryption_key(&self, name: &str) -> Result<SwapEncryptionKey, SwapError> {
        let credentials = self.runtime_dir.join("credentials");
        let path = credentials.join(format!("{name}-swap-key"));

        read_or_create_secret(&path).map(SwapEncryptionKey)
    }

    /// Returns durable paths below the ignored runtime directory.
    pub fn paths(&self, name: &str) -> SwapPaths {
        SwapPaths::under(self.runtime_dir.join("swaps"), name)
    }
}

fn runtime_dir() -> PathBuf {
    std::env::var("XMM_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_RUNTIME_DIR))
}

fn load_lnd(runtime_dir: &Path, name: &str, rest_url: &str) -> Result<LndRestClient, SwapError> {
    let directory = runtime_dir.join(name);
    let certificate = fs::read(directory.join("tls.cert"))
        .map_err(|error| SwapError::Durability(error.to_string()))?;
    let macaroon = fs::read_to_string(directory.join("admin.macaroon"))
        .map_err(|error| SwapError::Durability(error.to_string()))?;

    LndRestClient::new(LndRestConfig {
        base_url: rest_url.to_owned(),
        tls_certificate_pem: certificate,
        macaroon_hex: macaroon.trim().to_owned(),
        request_timeout: Duration::from_secs(30),
        network: LightningNetwork::Regtest,
    })
    .map_err(|error| SwapError::Lightning(error.to_string()))
}

fn required_env(name: &str) -> Result<String, SwapError> {
    std::env::var(name).map_err(|_| SwapError::InvalidInput(format!("{name} is required")))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, SwapError> {
    let bytes = fs::read(path).map_err(|error| SwapError::Durability(error.to_string()))?;

    serde_json::from_slice(&bytes)
        .map_err(|error| SwapError::Durability(format!("invalid fixture JSON: {error}")))
}

fn read_or_create_secret(path: &Path) -> Result<[u8; 32], SwapError> {
    if path.exists() {
        let bytes = fs::read(path).map_err(|error| SwapError::Durability(error.to_string()))?;

        return bytes
            .try_into()
            .map_err(|_| SwapError::Durability("live swap key has invalid length".to_owned()));
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| SwapError::Durability(error.to_string()))?;
    }

    let mut secret = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut secret);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }

    let mut file = options
        .open(path)
        .map_err(|error| SwapError::Durability(error.to_string()))?;
    file.write_all(&secret)
        .and_then(|_| file.sync_all())
        .map_err(|error| SwapError::Durability(error.to_string()))?;

    Ok(secret)
}
