#!/usr/bin/env bash
set -euo pipefail
umask 077

XMM_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${XMM_ROOT}"
export XMM_NETWORK=regtest

XMM_DEMO_DIR="$(mktemp -d runtime/market-maker-demo-XXXXXXXX)"
XMM_SESSION="$(basename "${XMM_DEMO_DIR}")"
XMM_TEST_ASSET="$(python3 -c 'import json; print(json.load(open("runtime/public-fixture.json"))["elements"]["testDepixAssetId"])')"
XMM_MAKER_A_PID=""
XMM_MAKER_B_PID=""

cleanup() {
  if [[ -n "${XMM_MAKER_A_PID}" ]]; then
    kill "${XMM_MAKER_A_PID}" 2>/dev/null || true
    wait "${XMM_MAKER_A_PID}" 2>/dev/null || true
  fi
  if [[ -n "${XMM_MAKER_B_PID}" ]]; then
    kill "${XMM_MAKER_B_PID}" 2>/dev/null || true
    wait "${XMM_MAKER_B_PID}" 2>/dev/null || true
  fi
}
trap cleanup EXIT

./target/debug/xmm solver keygen --out "${XMM_DEMO_DIR}/a.key" > "${XMM_DEMO_DIR}/a.public.json"
./target/debug/xmm solver keygen --out "${XMM_DEMO_DIR}/b.key" > "${XMM_DEMO_DIR}/b.public.json"
XMM_KEY_A="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["public_key"])' "${XMM_DEMO_DIR}/a.public.json")"
XMM_KEY_B="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["public_key"])' "${XMM_DEMO_DIR}/b.public.json")"

start_maker() {
  exec ./target/debug/xmm solver serve \
    --bind "$1" --solver-id "$2" --key-id "$3" \
    --key-file "$4" --database "$5" --asset-hash "${XMM_TEST_ASSET}" \
    --inventory-asset-id BTC --inventory-amount 1000000 \
    --rate-numerator 1 --rate-denominator 1 --fee-lbtc "$6"
}

start_maker 127.0.0.1:37771 maker-a key-a "${XMM_DEMO_DIR}/a.key" "${XMM_DEMO_DIR}/a.sqlite" 1000 > "${XMM_DEMO_DIR}/a.log" 2>&1 &
XMM_MAKER_A_PID=$!
start_maker 127.0.0.1:37772 maker-b key-b "${XMM_DEMO_DIR}/b.key" "${XMM_DEMO_DIR}/b.sqlite" 500 > "${XMM_DEMO_DIR}/b.log" 2>&1 &
XMM_MAKER_B_PID=$!

for XMM_ATTEMPT in {1..50}; do
  kill -0 "${XMM_MAKER_A_PID}" "${XMM_MAKER_B_PID}"
  if curl --silent --fail http://127.0.0.1:37771/health > /dev/null && \
     curl --silent --fail http://127.0.0.1:37772/health > /dev/null; then
    break
  fi
  sleep 0.1
done

kill -0 "${XMM_MAKER_A_PID}" "${XMM_MAKER_B_PID}"
curl --silent --fail http://127.0.0.1:37771/health > /dev/null
curl --silent --fail http://127.0.0.1:37772/health > /dev/null

./target/debug/xmm swap prepare \
  --session "${XMM_SESSION}" --intent-out "${XMM_DEMO_DIR}/intent.json" \
  --database "${XMM_DEMO_DIR}/client.sqlite" --recovery-key "${XMM_DEMO_DIR}/recovery.key" \
  --asset-hash "${XMM_TEST_ASSET}" --amount-in 1000 --amount-out 1000 \
  --fee-limit-lbtc 2000 --fee-sats 500 \
  --receiver-url https://127.0.0.1:29082 \
  --receiver-cert runtime/lnd-bob/tls.cert --receiver-macaroon runtime/lnd-bob/admin.macaroon \
  > "${XMM_DEMO_DIR}/prepared.json"

./target/debug/xmm rfq \
  --peer 127.0.0.1:37771,127.0.0.1:37772 --intent "${XMM_DEMO_DIR}/intent.json" \
  --pinned-key "key-a=${XMM_KEY_A},key-b=${XMM_KEY_B}" --asset-hash "${XMM_TEST_ASSET}" \
  --reserve --out "${XMM_DEMO_DIR}/selected.json" > "${XMM_DEMO_DIR}/rfq.json"

XMM_SWAP_ARGS=(
  --intent "${XMM_DEMO_DIR}/intent.json" --quote "${XMM_DEMO_DIR}/selected.json"
  --database "${XMM_DEMO_DIR}/client.sqlite" --recovery-key "${XMM_DEMO_DIR}/recovery.key"
  --pinned-key "key-a=${XMM_KEY_A},key-b=${XMM_KEY_B}" --asset-hash "${XMM_TEST_ASSET}"
  --solver-database "${XMM_DEMO_DIR}/b.sqlite"
  --receiver-url https://127.0.0.1:29082
  --receiver-cert runtime/lnd-bob/tls.cert --receiver-macaroon runtime/lnd-bob/admin.macaroon
  --payer-url https://127.0.0.1:29081
  --payer-cert runtime/lnd-alice/tls.cert --payer-macaroon runtime/lnd-alice/admin.macaroon
  --fee-sats 500
)

printf '%s\n' '$ xmm swap forward [prepared session and signed quote]'
./target/debug/xmm swap forward "${XMM_SWAP_ARGS[@]}"
printf '%s\n' '$ xmm swap resume [same session and signed quote]'
./target/debug/xmm swap resume "${XMM_SWAP_ARGS[@]}"
printf 'Private recovery retained at %s\n' "${XMM_DEMO_DIR}"
