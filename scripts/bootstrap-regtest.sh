#!/usr/bin/env bash
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/compose.sh"

assert_regtest_only

RUNTIME_DIR="$(runtime_dir)"
CREDENTIALS_DIR="$(credentials_dir)"
PUBLIC_FIXTURE="${RUNTIME_DIR}/public-fixture.json"
ASSET_ID_FILE="${RUNTIME_DIR}/test-depix-asset-id"
BITCOIN_RPC_USER="cross_network_market_maker"
BITCOIN_RPC_PASSWORD="cross_network_market_maker_rpc_password"
ELEMENTS_RPC_USER="cross_network_market_maker"
ELEMENTS_RPC_PASSWORD="cross_network_market_maker_elements_rpc_password"
BITCOIN_FAUCET_WALLET="cross_network_market_maker_faucet"
FAUCET_TOP_UP_SATS="100000000"
FAUCET_MIN_BALANCE_BTC="0.10000000"

mkdir -p "${CREDENTIALS_DIR}"
chmod 700 "${RUNTIME_DIR}" "${CREDENTIALS_DIR}"

lnd_host_port() {
  local service="$1"

  if [[ "${service}" == "lnd-alice" ]]; then
    printf '%s\n' "29081"
    return 0
  fi

  printf '%s\n' "29082"
}

lnd_tls_path() {
  local service="$1"

  printf '%s\n' "${RUNTIME_DIR}/${service}/tls.cert"
}

ensure_lnd_tls_certificate() {
  local service="$1"
  local path

  path="$(lnd_tls_path "${service}")"
  mkdir -p "$(dirname "${path}")"

  if [[ ! -f "${path}" ]]; then
    compose exec -T "${service}" cat /root/.lnd/tls.cert > "${path}"
    chmod 644 "${path}"
  fi
}

lnd_rest_curl() {
  local service="$1"
  local path="$2"
  local method="$3"
  local body="${4:-}"
  local port
  local tls_path

  port="$(lnd_host_port "${service}")"
  tls_path="$(lnd_tls_path "${service}")"
  ensure_lnd_tls_certificate "${service}"

  if [[ -n "${body}" ]]; then
    printf '%s' "${body}" | curl --silent --show-error --fail --cacert "${tls_path}" --request "${method}" --header 'Content-Type: application/json' --data-binary @- "https://127.0.0.1:${port}${path}"
    return 0
  fi

  curl --silent --show-error --fail --cacert "${tls_path}" --request "${method}" "https://127.0.0.1:${port}${path}"
}

password_json() {
  local password_file="$1"

  python3 - "${password_file}" <<'PY'
import base64
import json
import pathlib
import sys

password = pathlib.Path(sys.argv[1]).read_text().strip().encode()
print(json.dumps({"wallet_password": base64.b64encode(password).decode()}))
PY
}

init_wallet_body() {
  local password_file="$1"
  local seed_path="$2"

  python3 - "${password_file}" "${seed_path}" <<'PY'
import base64
import json
import pathlib
import sys

password = pathlib.Path(sys.argv[1]).read_text().strip().encode()
seed = json.loads(pathlib.Path(sys.argv[2]).read_text())
print(json.dumps({
    "wallet_password": base64.b64encode(password).decode(),
    "cipher_seed_mnemonic": seed["cipher_seed_mnemonic"],
}))
PY
}

wait_for_bitcoin() {
  local attempt

  for attempt in $(seq 1 60); do
    if compose exec -T bitcoin bitcoin-cli -regtest -datadir=/data/.bitcoin -rpcuser="${BITCOIN_RPC_USER}" -rpcpassword="${BITCOIN_RPC_PASSWORD}" -rpcconnect=127.0.0.1 -rpcport=18443 getblockchaininfo >/dev/null 2>&1; then
      return 0
    fi

    sleep 1
  done

  echo "Bitcoin Core did not become ready" >&2
  return 1
}

wait_for_elements() {
  local attempt

  for attempt in $(seq 1 60); do
    if compose exec -T elements elements-cli -chain=elementsregtest -datadir=/data/.elements -rpcuser="${ELEMENTS_RPC_USER}" -rpcpassword="${ELEMENTS_RPC_PASSWORD}" -rpcconnect=127.0.0.1 -rpcport=7041 getblockchaininfo >/dev/null 2>&1; then
      return 0
    fi

    sleep 1
  done

  echo "Elements did not become ready" >&2
  return 1
}

create_lnd_wallet() {
  local service="$1"
  local rpc_server="$2"
  local password_file="$3"
  local password
  local seed_path="${CREDENTIALS_DIR}/$(basename "${password_file%-wallet-password}")-cipher-seed.json"
  local wallet_state
  local init_body

  if [[ -f "${password_file}" ]]; then
    password="$(cat "${password_file}")"

    if compose exec -T "${service}" lncli --network=regtest --rpcserver="${rpc_server}" getinfo >/dev/null 2>&1; then
      return 0
    fi

    if lnd_rest_curl "${service}" /v1/unlockwallet POST "$(password_json "${password_file}")" >/dev/null 2>&1; then
      return 0
    fi
  else
    password="$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
    umask 077
    printf '%s\n' "${password}" > "${password_file}"
    chmod 600 "${password_file}"
  fi

  wallet_state="$(lnd_rest_curl "${service}" /v1/state GET | python3 -c 'import json,sys; print(json.load(sys.stdin).get("state", ""))' || true)"

  if [[ "${wallet_state}" == "UNLOCKED" ]]; then
    return 0
  fi

  if [[ "${wallet_state}" == "LOCKED" ]]; then
    lnd_rest_curl "${service}" /v1/unlockwallet POST "$(password_json "${password_file}")" >/dev/null
    return 0
  fi

  if [[ "${wallet_state}" != "NON_EXISTING" ]]; then
    echo "${service} wallet state is unavailable or unexpected" >&2
    return 1
  fi

  if [[ ! -f "${seed_path}" ]]; then
    umask 077
    lnd_rest_curl "${service}" /v1/genseed GET > "${seed_path}"
    chmod 600 "${seed_path}"
  fi

  init_body="$(init_wallet_body "${password_file}" "${seed_path}")"
  lnd_rest_curl "${service}" /v1/initwallet POST "${init_body}" >/dev/null
}

extract_lnd_credentials() {
  local service="$1"
  local macaroon_path
  local macaroon_hex
  local service_dir="${RUNTIME_DIR}/${service}"

  mkdir -p "${service_dir}"
  ensure_lnd_tls_certificate "${service}"
  macaroon_path="${service_dir}/admin.macaroon"
  macaroon_hex="$(compose exec -T "${service}" cat /root/.lnd/data/chain/bitcoin/regtest/admin.macaroon | python3 -c 'import sys; print(sys.stdin.buffer.read().hex())')"
  printf '%s\n' "${macaroon_hex}" > "${macaroon_path}"
  chmod 600 "${macaroon_path}"
}

lnd_value() {
  local service="$1"
  local rpc_server="$2"

  shift 2

  compose exec -T "${service}" lncli --network=regtest --rpcserver="${rpc_server}" "$@"
}

bitcoin_cli() {
  compose exec -T bitcoin bitcoin-cli -regtest -datadir=/data/.bitcoin -rpcuser="${BITCOIN_RPC_USER}" -rpcpassword="${BITCOIN_RPC_PASSWORD}" -rpcconnect=127.0.0.1 -rpcport=18443 "$@"
}

bitcoin_faucet_cli() {
  bitcoin_cli "-rpcwallet=${BITCOIN_FAUCET_WALLET}" "$@"
}

elements_cli() {
  compose exec -T elements elements-cli -chain=elementsregtest -datadir=/data/.elements -rpcuser="${ELEMENTS_RPC_USER}" -rpcpassword="${ELEMENTS_RPC_PASSWORD}" -rpcconnect=127.0.0.1 -rpcport=7041 "$@"
}

json_value() {
  local path="$1"

  python3 -c 'import json,sys
value=json.load(sys.stdin)
for name in sys.argv[1].split("."):
    value=value[name]
print(value)' "${path}"
}

json_optional_value() {
  local name="$1"

  python3 -c 'import json,sys
value=json.load(sys.stdin)
print(value.get(sys.argv[1], ""))' "${name}"
}

decimal_at_least() {
  local value="$1"
  local minimum="$2"

  python3 - "${value}" "${minimum}" <<'PY'
from decimal import Decimal
import sys

raise SystemExit(0 if Decimal(sys.argv[1]) >= Decimal(sys.argv[2]) else 1)
PY
}

decimal_positive() {
  local value="$1"

  python3 - "${value}" <<'PY'
from decimal import Decimal
import sys

raise SystemExit(0 if Decimal(sys.argv[1]) > Decimal("0") else 1)
PY
}

faucet_trusted_balance() {
  bitcoin_faucet_cli getbalances | python3 -c 'import json,sys
value=json.load(sys.stdin)
print(value.get("mine", {}).get("trusted", "0"))'
}

faucet_pending_balance() {
  bitcoin_faucet_cli getbalances | python3 -c 'import json,sys
value=json.load(sys.stdin)
print(value.get("mine", {}).get("untrusted_pending", "0"))'
}

ensure_bitcoin_faucet_wallet() {
  if bitcoin_faucet_cli getwalletinfo >/dev/null 2>&1; then
    return 0
  fi

  if bitcoin_cli loadwallet "${BITCOIN_FAUCET_WALLET}" >/dev/null 2>&1; then
    return 0
  fi

  bitcoin_cli createwallet "${BITCOIN_FAUCET_WALLET}" false false "" false true true false >/dev/null
}

confirm_bitcoin_faucet_transfer() {
  local txid="${1:-}"
  local miner_address
  local confirmations

  miner_address="$(bitcoin_faucet_cli getnewaddress "cross-network-market-maker-faucet-confirmation" bech32)"
  bitcoin_cli generatetoaddress 1 "${miner_address}" >/dev/null

  if [[ -n "${txid}" ]]; then
    confirmations="$(bitcoin_faucet_cli gettransaction "${txid}" | json_value confirmations)"

    if [[ "${confirmations}" -lt 1 ]]; then
      echo "Bitcoin faucet transfer did not confirm" >&2
      return 1
    fi
  fi

  wait_for_lnd_sync lnd-alice lnd-alice:10009
  wait_for_lnd_sync lnd-bob lnd-bob:10009

  if ! decimal_at_least "$(faucet_trusted_balance)" "${FAUCET_MIN_BALANCE_BTC}"; then
    echo "Bitcoin faucet trusted balance is below ${FAUCET_MIN_BALANCE_BTC} BTC" >&2
    return 1
  fi
}

ensure_bitcoin_faucet_balance() {
  local trusted_balance
  local pending_balance

  ensure_bitcoin_faucet_wallet
  trusted_balance="$(faucet_trusted_balance)"

  if decimal_at_least "${trusted_balance}" "${FAUCET_MIN_BALANCE_BTC}"; then
    return 0
  fi

  pending_balance="$(faucet_pending_balance)"

  if decimal_positive "${pending_balance}"; then
    confirm_bitcoin_faucet_transfer
    return 0
  fi

  local faucet_address
  local send_result
  local txid

  faucet_address="$(bitcoin_faucet_cli getnewaddress "cross-network-market-maker-faucet-top-up" bech32)"
  send_result="$(lnd_value lnd-alice lnd-alice:10009 sendcoins --addr="${faucet_address}" --amt="${FAUCET_TOP_UP_SATS}" --sat_per_vbyte=1 --min_confs=1 --force)"
  txid="$(printf '%s' "${send_result}" | json_value txid)"

  if [[ -z "${txid}" ]]; then
    echo "Alice LND did not return a Bitcoin faucet transfer id" >&2
    return 1
  fi

  confirm_bitcoin_faucet_transfer "${txid}"
}

wait_for_lnd_sync() {
  local service="$1"
  local rpc_server="$2"
  local attempt

  for attempt in $(seq 1 90); do
    if lnd_value "${service}" "${rpc_server}" getinfo | python3 -c 'import json,sys; value=json.load(sys.stdin); raise SystemExit(0 if value.get("synced_to_chain") else 1)' >/dev/null 2>&1; then
      return 0
    fi

    sleep 1
  done

  echo "${service} did not synchronize to Bitcoin regtest" >&2
  return 1
}

has_local_channel() {
  local service="$1"
  local rpc_server="$2"
  local peer_pubkey="$3"

  if lnd_value "${service}" "${rpc_server}" listchannels --public_only=false | python3 -c 'import json,sys
payload=json.load(sys.stdin)
peer=sys.argv[1]
channels=payload.get("channels", [])
raise SystemExit(0 if any(channel.get("remote_pubkey") == peer and channel.get("initiator") is True for channel in channels) else 1)' "${peer_pubkey}"
  then
    return 0
  fi

  if lnd_value "${service}" "${rpc_server}" pendingchannels | python3 -c 'import json,sys
payload=json.load(sys.stdin)
peer=sys.argv[1]
channels=payload.get("pending_open_channels", [])
raise SystemExit(0 if any(channel.get("channel", {}).get("remote_node_pub") == peer and channel.get("channel", {}).get("initiator") == "INITIATOR_LOCAL" for channel in channels) else 1)' "${peer_pubkey}"
  then
    return 0
  fi

  return 1
}

ensure_elements_wallet() {
  if elements_cli -rpcwallet=cross_network_market_maker getwalletinfo >/dev/null 2>&1; then
    return 0
  fi

  if elements_cli loadwallet cross_network_market_maker >/dev/null 2>&1; then
    return 0
  fi

  elements_cli createwallet cross_network_market_maker >/dev/null
}

ensure_explicit_liquidity() {
  local test_asset_id="$1"
  local policy_asset_id="$2"
  local mining_address="$3"
  local counts
  local asset_count
  local fee_count

  counts="$(elements_cli -rpcwallet=cross_network_market_maker listunspent 1 999999 | python3 -c 'import json,sys
payload=json.load(sys.stdin)
asset_id=sys.argv[1]
policy_id=sys.argv[2]
explicit=lambda entry: not isinstance(entry.get("amountcommitment"),str) and not isinstance(entry.get("assetcommitment"),str)
print(sum(1 for entry in payload if entry.get("asset")==asset_id and explicit(entry)),sum(1 for entry in payload if entry.get("asset")==policy_id and explicit(entry)))' "${test_asset_id}" "${policy_asset_id}")"
  read -r asset_count fee_count <<< "${counts}"

  if [[ "${asset_count:-0}" -ge 4 && "${fee_count:-0}" -ge 8 ]]; then
    return 0
  fi

  local addresses=()
  local index

  for index in $(seq 1 12); do
    local confidential_address
    local unconfidential_address

    confidential_address="$(elements_cli -rpcwallet=cross_network_market_maker getnewaddress)"
    unconfidential_address="$(elements_cli -rpcwallet=cross_network_market_maker getaddressinfo "${confidential_address}" | python3 -c 'import json,sys; value=json.load(sys.stdin); print(value.get("unconfidential") or value.get("unconfidential_address", ""))')"

    if [[ -z "${unconfidential_address}" ]]; then
      echo "Elements did not return an unconfidential wallet address" >&2
      return 1
    fi

    addresses+=("${unconfidential_address}")
  done

  local amounts
  local output_assets

  read -r amounts output_assets < <(python3 - "${test_asset_id}" "${policy_asset_id}" "${addresses[@]}" <<'PY'
import json
import sys

asset_id = sys.argv[1]
policy_id = sys.argv[2]
addresses = sys.argv[3:]
amounts = {address: "0.00050000" for address in addresses}
output_assets = {address: asset_id if index < 4 else policy_id for index, address in enumerate(addresses)}
print(json.dumps(amounts, separators=(",", ":")), json.dumps(output_assets, separators=(",", ":")))
PY
  )

  elements_cli -rpcwallet=cross_network_market_maker sendmany "" "${amounts}" 1 "cross-network-market-maker-bootstrap" '[]' false 1 ECONOMICAL "${output_assets}" true >/dev/null
  elements_cli -rpcwallet=cross_network_market_maker generatetoaddress 1 "${mining_address}" >/dev/null
}

ensure_bitcoin_tip() {
  local blocks
  local alice_address

  blocks="$(bitcoin_cli getblockchaininfo | json_value blocks)"

  if [[ "${blocks}" != "0" ]]; then
    return 0
  fi

  alice_address="$(lnd_value lnd-alice lnd-alice:10009 newaddress p2wkh | json_value address)"
  bitcoin_cli generatetoaddress 1 "${alice_address}" >/dev/null
}

ensure_lnd_channel() {
  local alice_info
  local bob_info
  local alice_pubkey
  local bob_pubkey
  local alice_address

  alice_info="$(lnd_value lnd-alice lnd-alice:10009 getinfo)"
  bob_info="$(lnd_value lnd-bob lnd-bob:10009 getinfo)"
  alice_pubkey="$(printf '%s' "${alice_info}" | json_value identity_pubkey)"
  bob_pubkey="$(printf '%s' "${bob_info}" | json_value identity_pubkey)"
  alice_address="$(lnd_value lnd-alice lnd-alice:10009 newaddress p2wkh | json_value address)"

  compose exec -T lnd-alice lncli --network=regtest --rpcserver=lnd-alice:10009 connect "${bob_pubkey}@lnd-bob:9735" >/dev/null 2>&1 || true

  if ! has_local_channel lnd-alice lnd-alice:10009 "${bob_pubkey}"; then
    compose exec -T lnd-alice lncli --network=regtest --rpcserver=lnd-alice:10009 openchannel --private --local_amt=200000 --push_amt=100000 --sat_per_vbyte=1 "${bob_pubkey}" >/dev/null
    bitcoin_cli generatetoaddress 6 "${alice_address}" >/dev/null
    wait_for_lnd_sync lnd-alice lnd-alice:10009
    wait_for_lnd_sync lnd-bob lnd-bob:10009
  fi

  if ! has_local_channel lnd-bob lnd-bob:10009 "${alice_pubkey}"; then
    compose exec -T lnd-bob lncli --network=regtest --rpcserver=lnd-bob:10009 openchannel --private --local_amt=200000 --push_amt=100000 --sat_per_vbyte=1 "${alice_pubkey}" >/dev/null
    bitcoin_cli generatetoaddress 6 "${alice_address}" >/dev/null
    wait_for_lnd_sync lnd-alice lnd-alice:10009
    wait_for_lnd_sync lnd-bob lnd-bob:10009
  fi
}

write_public_marker() {
  local path="$1"
  local value="$2"

  mkdir -p "$(dirname "${path}")"
  printf '%s\n' "${value}" > "${path}"
  chmod 644 "${path}"
}

if [[ -f "${PUBLIC_FIXTURE}" ]]; then
  wait_for_bitcoin
  wait_for_elements
  create_lnd_wallet lnd-alice lnd-alice:10009 "${CREDENTIALS_DIR}/alice-wallet-password"
  create_lnd_wallet lnd-bob lnd-bob:10009 "${CREDENTIALS_DIR}/bob-wallet-password"
  extract_lnd_credentials lnd-alice
  extract_lnd_credentials lnd-bob
  ensure_elements_wallet
  elements_cli -rpcwallet=cross_network_market_maker rescanblockchain 0 >/dev/null
  existing_test_asset_id="$(json_value elements.testDepixAssetId < "${PUBLIC_FIXTURE}")"
  existing_policy_asset_id="$(json_value elements.policyAssetId < "${PUBLIC_FIXTURE}")"
  existing_elements_address="$(elements_cli -rpcwallet=cross_network_market_maker getnewaddress)"
  ensure_explicit_liquidity "${existing_test_asset_id}" "${existing_policy_asset_id}" "${existing_elements_address}"
  ensure_bitcoin_faucet_balance
  ensure_bitcoin_tip
  wait_for_lnd_sync lnd-alice lnd-alice:10009
  wait_for_lnd_sync lnd-bob lnd-bob:10009
  ensure_lnd_channel
  printf '%s\n' "${PUBLIC_FIXTURE}"
  exit 0
fi

wait_for_bitcoin
wait_for_elements

create_lnd_wallet lnd-alice lnd-alice:10009 "${CREDENTIALS_DIR}/alice-wallet-password"
create_lnd_wallet lnd-bob lnd-bob:10009 "${CREDENTIALS_DIR}/bob-wallet-password"
extract_lnd_credentials lnd-alice
extract_lnd_credentials lnd-bob

alice_info="$(lnd_value lnd-alice lnd-alice:10009 getinfo)"
bob_info="$(lnd_value lnd-bob lnd-bob:10009 getinfo)"
alice_pubkey="$(printf '%s' "${alice_info}" | json_value identity_pubkey)"
bob_pubkey="$(printf '%s' "${bob_info}" | json_value identity_pubkey)"

alice_address="$(lnd_value lnd-alice lnd-alice:10009 newaddress p2wkh | json_value address)"
bob_address="$(lnd_value lnd-bob lnd-bob:10009 newaddress p2wkh | json_value address)"

bitcoin_cli generatetoaddress 101 "${alice_address}" >/dev/null
bitcoin_cli generatetoaddress 101 "${bob_address}" >/dev/null

wait_for_lnd_sync lnd-alice lnd-alice:10009
wait_for_lnd_sync lnd-bob lnd-bob:10009
ensure_bitcoin_faucet_balance

compose exec -T lnd-alice lncli --network=regtest --rpcserver=lnd-alice:10009 connect "${bob_pubkey}@lnd-bob:9735" >/dev/null 2>&1 || true

if ! has_local_channel lnd-alice lnd-alice:10009 "${bob_pubkey}"; then
  compose exec -T lnd-alice lncli --network=regtest --rpcserver=lnd-alice:10009 openchannel --private --local_amt=200000 --push_amt=100000 --sat_per_vbyte=1 "${bob_pubkey}" >/dev/null
  bitcoin_cli generatetoaddress 6 "${alice_address}" >/dev/null
  wait_for_lnd_sync lnd-alice lnd-alice:10009
  wait_for_lnd_sync lnd-bob lnd-bob:10009
fi

if ! has_local_channel lnd-bob lnd-bob:10009 "${alice_pubkey}"; then
  compose exec -T lnd-bob lncli --network=regtest --rpcserver=lnd-bob:10009 openchannel --private --local_amt=200000 --push_amt=100000 --sat_per_vbyte=1 "${alice_pubkey}" >/dev/null
  bitcoin_cli generatetoaddress 6 "${alice_address}" >/dev/null
  wait_for_lnd_sync lnd-alice lnd-alice:10009
  wait_for_lnd_sync lnd-bob lnd-bob:10009
fi

ensure_elements_wallet

elements_cli -rpcwallet=cross_network_market_maker rescanblockchain 0 >/dev/null
elements_address="$(elements_cli -rpcwallet=cross_network_market_maker getnewaddress)"
elements_cli generatetoaddress 101 "${elements_address}" >/dev/null
if [[ -f "${ASSET_ID_FILE}" ]]; then
  test_depix_asset_id="$(cat "${ASSET_ID_FILE}")"
else
  asset_result="$(elements_cli -rpcwallet=cross_network_market_maker issueasset 100000 1)"
  test_depix_asset_id="$(printf '%s' "${asset_result}" | json_value asset)"
  write_public_marker "${ASSET_ID_FILE}" "${test_depix_asset_id}"
  elements_cli generatetoaddress 6 "${elements_address}" >/dev/null
fi

policy_asset_id="$(elements_cli getsidechaininfo | json_optional_value pegged_asset)"

if [[ -z "${policy_asset_id}" ]]; then
  echo "Elements did not report its regtest policy asset" >&2
  exit 1
fi

ensure_explicit_liquidity "${test_depix_asset_id}" "${policy_asset_id}" "${elements_address}"

mkdir -p "${RUNTIME_DIR}"
chmod 700 "${RUNTIME_DIR}"
python3 - "${PUBLIC_FIXTURE}" "${alice_pubkey}" "${bob_pubkey}" "${alice_address}" "${bob_address}" "${test_depix_asset_id}" "${policy_asset_id}" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
fixture = {
    "network": "regtest",
    "bitcoin": {
        "rpcUrl": "http://127.0.0.1:29443",
        "rpcUser": "cross_network_market_maker",
        "faucetWallet": "cross_network_market_maker_faucet",
    },
    "elements": {
        "rpcUrl": "http://127.0.0.1:27051",
        "rpcUser": "cross_network_market_maker",
        "wallet": "cross_network_market_maker",
        "network": "elementsregtest",
        "policyAssetId": sys.argv[7],
        "testDepixAssetId": sys.argv[6],
    },
    "lightning": {
        "alice": {
            "restUrl": "https://127.0.0.1:29081",
            "grpcUrl": "https://127.0.0.1:22009",
            "identityPubkey": sys.argv[2],
            "address": sys.argv[4],
        },
        "bob": {
            "restUrl": "https://127.0.0.1:29082",
            "grpcUrl": "https://127.0.0.1:22010",
            "identityPubkey": sys.argv[3],
            "address": sys.argv[5],
        },
    },
}
path.write_text(json.dumps(fixture, indent=2) + "\n")
path.chmod(0o644)
PY

printf '%s\n' "${PUBLIC_FIXTURE}"
