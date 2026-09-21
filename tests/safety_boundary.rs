use std::process::Command;
use std::time::Duration;

use cross_network_market_maker::core::settlement::SwapState;
use cross_network_market_maker::lightning::{LightningNetwork, LndRestClient, LndRestConfig};
use cross_network_market_maker::rpc::RpcClient;

#[test]
fn cli_rejects_mainnet_before_creating_solver_material() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let key_path = directory.path().join("must-not-exist.key");
    let output = Command::new(env!("CARGO_BIN_EXE_xmm"))
        .env("XMM_NETWORK", "mainnet")
        .args(["solver", "keygen", "--out"])
        .arg(&key_path)
        .output()
        .expect("run binary");

    assert!(!output.status.success());
    assert!(!key_path.exists());
}

#[test]
fn rpc_rejects_remote_and_credential_bearing_endpoints() {
    for endpoint in [
        "http://example.com:8332",
        "https://127.0.0.1.example.com:8332",
        "http://user:secret@127.0.0.1:29443",
        "http://127.0.0.1:29443?token=secret",
        "http://127.0.0.1:29443#secret",
    ] {
        assert!(
            RpcClient::new(
                endpoint,
                "local-user",
                "local-secret",
                Duration::from_secs(1)
            )
            .is_err(),
            "unsafe endpoint was accepted: {endpoint}"
        );
    }
}

#[test]
fn rpc_debug_omits_authentication_material() {
    let rpc = RpcClient::new(
        "http://127.0.0.1:29443",
        "debug-test-user",
        "debug-test-secret",
        Duration::from_secs(1),
    )
    .expect("local client");
    let debug = format!("{rpc:?}");

    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("debug-test-user"));
    assert!(!debug.contains("debug-test-secret"));
    assert!(!debug.contains("Basic "));
}

#[test]
fn lnd_rejects_insecure_or_remote_endpoints_before_certificate_loading() {
    for base_url in [
        "http://127.0.0.1:29081",
        "https://example.com:29081",
        "https://127.0.0.1:29081?macaroon=secret",
        "https://user:secret@127.0.0.1:29081",
    ] {
        let result = LndRestClient::new(LndRestConfig {
            base_url: base_url.to_owned(),
            tls_certificate_pem: Vec::new(),
            macaroon_hex: String::new(),
            request_timeout: Duration::from_secs(1),
            network: LightningNetwork::Regtest,
        });
        let error = result
            .err()
            .expect("invalid endpoint is rejected")
            .to_string();

        assert!(!error.contains("certificate"));
    }
}

#[test]
fn paid_states_cannot_switch_to_refund_or_retry_payment() {
    for state in [
        SwapState::OutputSettled,
        SwapState::ClaimPending,
        SwapState::Settled,
    ] {
        assert!(!state.can_transition_to(SwapState::Refunding));
        assert!(!state.can_transition_to(SwapState::OutputPending));
    }
}
