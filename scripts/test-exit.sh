#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/compose.sh"

assert_regtest_only

if [[ "${CROSS_NETWORK_MARKET_MAKER_EXIT_QUALIFIED:-}" != "1" ]]; then
  echo "Native exit is an explicit regtest qualification opt-in; set CROSS_NETWORK_MARKET_MAKER_EXIT_QUALIFIED=1 to run it" >&2
  exit 64
fi

XMM_BITCOIN_RPC_PASSWORD="${XMM_BITCOIN_RPC_PASSWORD:-cross_network_market_maker_rpc_password}"
XMM_ELEMENTS_RPC_PASSWORD="${XMM_ELEMENTS_RPC_PASSWORD:-cross_network_market_maker_elements_rpc_password}"

export XMM_NETWORK=regtest
export XMM_BITCOIN_RPC_PASSWORD
export XMM_ELEMENTS_RPC_PASSWORD
export XMM_RUN_LIVE_LIGHTNING_EXIT=1

XMM_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${XMM_ROOT}"

XMM_NETWORK=regtest ./scripts/bootstrap-regtest.sh
cargo test --locked --test live_exit -- --ignored --test-threads=1 --nocapture
