#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/compose.sh"

assert_regtest_only

if [[ "${CROSS_NETWORK_MARKET_MAKER_CONFIRM_RESET:-}" != "1" ]]; then
  echo "Set CROSS_NETWORK_MARKET_MAKER_CONFIRM_RESET=1 to remove only cross-network-market-maker containers, network, and volumes" >&2
  exit 64
fi

compose down --volumes --remove-orphans

rm -rf "$(runtime_dir)"
