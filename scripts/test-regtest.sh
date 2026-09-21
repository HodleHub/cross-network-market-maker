#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/compose.sh"

assert_regtest_only

XMM_BITCOIN_RPC_PASSWORD="${XMM_BITCOIN_RPC_PASSWORD:-cross_network_market_maker_rpc_password}"
XMM_ELEMENTS_RPC_PASSWORD="${XMM_ELEMENTS_RPC_PASSWORD:-cross_network_market_maker_elements_rpc_password}"

export XMM_NETWORK=regtest
export XMM_BITCOIN_RPC_PASSWORD
export XMM_ELEMENTS_RPC_PASSWORD
export XMM_RUN_LIVE=1
export XMM_RUN_LIVE_CHAIN=1
export XMM_RUN_LIVE_FORWARD=1
export XMM_RUN_LIVE_LIQUID=1
export XMM_RUN_LIVE_LBTC=1
export XMM_LIGHTNING_INTEGRATION=1
export XMM_RUN_LIVE_SWAP=1
export XMM_SWAP_INTEGRATION=1
export XMM_SWAP_FAILURE_INTEGRATION=1

XMM_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${XMM_ROOT}"

XMM_NETWORK=regtest ./scripts/bootstrap-regtest.sh

cargo test --locked --test live_chain -- --ignored --test-threads=1 --nocapture

XMM_NETWORK=regtest ./scripts/bootstrap-regtest.sh

cargo test --locked \
  --test live_lightning \
  --test live_swaps \
  --test live_forward \
  -- --ignored --test-threads=1 --nocapture
