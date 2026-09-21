#!/usr/bin/env bash
set -euo pipefail

XMM_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${XMM_ROOT}/infra/docker-compose.regtest.yml"
COMPOSE_PROJECT_NAME="cross-network-market-maker"

export COMPOSE_PROJECT_NAME

compose() {
  docker compose \
    --project-directory "${XMM_ROOT}" \
    --env-file /dev/null \
    --file "${COMPOSE_FILE}" \
    "$@"
}

assert_regtest_only() {
  local requested_network="${CROSS_NETWORK_MARKET_MAKER_NETWORK:-${XMM_NETWORK:-regtest}}"

  if [[ "${requested_network}" != "regtest" ]]; then
    echo "cross-network-market-maker refuses network=${requested_network}; only regtest is allowed" >&2
    return 64
  fi
}

runtime_dir() {
  printf '%s\n' "${XMM_ROOT}/runtime"
}

credentials_dir() {
  printf '%s\n' "$(runtime_dir)/credentials"
}
