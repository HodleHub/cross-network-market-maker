# Cross-network market maker tutorial

This walkthrough uses two independent local quote services and a shared regtest
settlement harness. It demonstrates the protocol and recovery flow with actual
nodes. Separate participant wallets, remote maker execution, permissionless
discovery, and real market prices are later implementation stages.

## 1. Understand the two inventories

For TEST-DEPIX → Lightning, the taker contributes TEST-DEPIX and the maker must
be able to deliver BTC over Lightning. The maker reserves its **destination BTC
inventory**. The taker also needs LBTC for Elements transaction fees.

For Lightning → TEST-DEPIX, the taker contributes BTC and the maker supplies the
TEST-DEPIX HTLC plus LBTC fees. Reserve TEST-DEPIX inventory for that direction.
Do not add values from different assets to calculate a common balance. A fixed
integer conversion ratio is used only to make these tests reproducible.

## 2. Build and bootstrap a small local chain

From the repository root:

```sh
cargo build --locked
./scripts/start-regtest.sh
./scripts/bootstrap-regtest.sh
./scripts/status-regtest.sh
```

The supplied Docker profile uses Linux ARM64 images and loopback API ports.
Bootstrap creates dedicated wallets, a synthetic TEST-DEPIX issuance, and an
Alice/Bob Lightning channel. The published fixture is
`runtime/public-fixture.json`. Credentials and recovery material live in the
ignored `runtime/` directory; never copy that directory into Git or evidence.

No public Bitcoin history is synchronized. The first run downloads software
images and compiles Rust, which still requires disk space. Stop preserves local
state, while reset destroys only the explicitly selected test fixture.

## 3. Inspect routes and run the protocol checks

```sh
./target/debug/xmm routes
cargo test --locked
```

The catalogue contains twenty directed routes. An unsupported route must fail
closed; its catalogue presence is not an execution guarantee. Normal Cargo
tests exercise protocol, persistence, and transaction construction. Real-node
tests are selected separately below.

## 4. Create two maker identities

```sh
umask 077
mkdir -p runtime/makers/a runtime/makers/b
./target/debug/xmm solver keygen --out runtime/makers/a/signing.key > runtime/makers/a/public.json
./target/debug/xmm solver keygen --out runtime/makers/b/signing.key > runtime/makers/b/public.json
```

Each `public.json` contains a public key for pinning; the `.key` file is private.
Key generation refuses to overwrite an existing key. On a resumed run, reuse
the existing keys and databases. Renaming a maker is not a key rotation protocol.

Read the locally issued asset identifier:

```sh
XMM_TEST_ASSET="$(python3 -c 'import json; print(json.load(open("runtime/public-fixture.json"))["elements"]["testDepixAssetId"])')"
```

Set that variable in both maker terminals. Run maker A in one terminal:

```sh
./target/debug/xmm solver serve \
  --bind 127.0.0.1:37771 \
  --solver-id maker-a --key-id key-a \
  --key-file runtime/makers/a/signing.key \
  --database runtime/makers/a/ledger.sqlite \
  --asset-hash "$XMM_TEST_ASSET" \
  --inventory-asset-id BTC --inventory-amount 1000000 \
  --rate-numerator 1 --rate-denominator 1 --fee-lbtc 1000
```

Run maker B in a second terminal with its own key, port, and ledger:

```sh
./target/debug/xmm solver serve \
  --bind 127.0.0.1:37772 \
  --solver-id maker-b --key-id key-b \
  --key-file runtime/makers/b/signing.key \
  --database runtime/makers/b/ledger.sqlite \
  --asset-hash "$XMM_TEST_ASSET" \
  --inventory-asset-id BTC --inventory-amount 1000000 \
  --rate-numerator 1 --rate-denominator 1 --fee-lbtc 500
```

These amounts are test bookkeeping. Declaring an inventory balance does not
prove that a remote operator has liquidity. A production maker must reconcile
its ledger with its own wallet, channel capacity, pending HTLCs, and reservations.

This first RFQ fixes both the input lot and the Lightning invoice amount, so the
two example makers compete on the signed fee field. It does not implement a
live FX order book. The quoted `fee_lbtc` is currently a negotiation/ranking
field; the runner's explicit `fee_sats` controls the Elements miner fee under the
signed cap. There is no separate maker-fee payout output or realized-spread
accounting in this POC.

For an automated version of steps 4–7 after bootstrap, run
`./scripts/demo-market-makers.sh`. It starts its own two maker processes on
ports 37771/37772, executes a fresh swap and replay, stops those processes, and
preserves the private session under `runtime/`. Do not run it alongside the
manual makers on those same ports.

## 5. Prepare a fresh exact-amount intent

In a third terminal, set the same `XMM_TEST_ASSET` value from step 4. Create a
unique session and retain these variables for the remaining commands:

```sh
umask 077
XMM_SESSION="tutorial-$(date +%s)"
XMM_SESSION_DIR="runtime/tutorial/$XMM_SESSION"
mkdir -p "$XMM_SESSION_DIR"
XMM_KEY_A="$(python3 -c 'import json; print(json.load(open("runtime/makers/a/public.json"))["public_key"])')"
XMM_KEY_B="$(python3 -c 'import json; print(json.load(open("runtime/makers/b/public.json"))["public_key"])')"

./target/debug/xmm swap prepare \
  --session "$XMM_SESSION" \
  --intent-out "$XMM_SESSION_DIR/intent.json" \
  --database "$XMM_SESSION_DIR/client.sqlite" \
  --recovery-key "$XMM_SESSION_DIR/recovery.key" \
  --asset-hash "$XMM_TEST_ASSET" \
  --amount-in 1000 --amount-out 1000 \
  --fee-limit-lbtc 2000 --fee-sats 500 \
  --receiver-url https://127.0.0.1:29082 \
  --receiver-cert runtime/lnd-bob/tls.cert \
  --receiver-macaroon runtime/lnd-bob/admin.macaroon
```

Preparation creates fresh secret material, an encrypted recovery record, and a
real Bob hold invoice for 1,000 regtest satoshis. The public intent commits to
that invoice, the asset hash, keys, destinations, and refund policy. Its validity
window is short: obtain the quote and start the swap promptly. Use `resume` for
an accepted session; do not repeat preparation to change its terms.

## 6. Request, verify, and reserve a quote

```sh
./target/debug/xmm rfq \
  --peer 127.0.0.1:37771,127.0.0.1:37772 \
  --intent "$XMM_SESSION_DIR/intent.json" \
  --pinned-key "key-a=$XMM_KEY_A,key-b=$XMM_KEY_B" \
  --asset-hash "$XMM_TEST_ASSET" \
  --reserve --out "$XMM_SESSION_DIR/selected.json"
```

With the example fees, the expected result selects **maker B**, the second peer,
and reserves its BTC inventory. The client verifies signatures and invoice
bindings before selecting the offer. The selected JSON retains the complete
signed quote and reservation identifier for recovery.

## 7. Execute and replay the same swap

The following Bash/Zsh array avoids repeating node and session arguments. The
database must belong to the selected maker; these example prices select B.

```sh
XMM_SWAP_ARGS=(
  --intent "$XMM_SESSION_DIR/intent.json"
  --quote "$XMM_SESSION_DIR/selected.json"
  --database "$XMM_SESSION_DIR/client.sqlite"
  --recovery-key "$XMM_SESSION_DIR/recovery.key"
  --pinned-key "key-a=$XMM_KEY_A,key-b=$XMM_KEY_B"
  --asset-hash "$XMM_TEST_ASSET"
  --solver-database runtime/makers/b/ledger.sqlite
  --receiver-url https://127.0.0.1:29082
  --receiver-cert runtime/lnd-bob/tls.cert
  --receiver-macaroon runtime/lnd-bob/admin.macaroon
  --payer-url https://127.0.0.1:29081
  --payer-cert runtime/lnd-alice/tls.cert
  --payer-macaroon runtime/lnd-alice/admin.macaroon
  --fee-sats 500
)

./target/debug/xmm swap forward "${XMM_SWAP_ARGS[@]}"
./target/debug/xmm swap resume "${XMM_SWAP_ARGS[@]}"
./target/debug/xmm swap status \
  --database "$XMM_SESSION_DIR/client.sqlite" --swap-id "$XMM_SESSION"
```

Success reports `SETTLED`, the funding and claim transaction IDs, verified
preimage linkage, and consumed maker inventory. Replay must return the same
transactions and must not initiate another Lightning payment. The local runner
opens the selected maker's SQLite database as part of the shared harness; a
remote production maker would own that reconciliation itself.

To exercise interruption, prepare a **fresh** session and quote using steps
5–6, then add `--stop-after-payment` to its first `swap forward` call. That call
intentionally exits with an error after verified payment and before claim.
Inspect `swap status`, then call `swap resume` with the same files and without
the stop flag. Do not treat the injected error as authorization to pay again.

## Recovery principles

Keep the session directory, encryption key, selected signed quote, client
database, and signed transaction outboxes together in private backup. Resume
the same session after interruption. Do not generate a new payment hash or
invoice to work around an unknown result.

Once Lightning reports a verified success, recovery finishes or reconciles the
claim. It must not initiate another payment or automatically refund the funded
asset. Refund eligibility after a failed attempt requires actual node states,
the contract deadline, and an unspent refund branch.

## Run settlement qualification and native exit

The regular suite also exercises Lightning → TEST-DEPIX, durable reverse replay,
and failure recovery through the Rust adapter API. The interactive signed-RFQ
CLI in this milestone covers TEST-DEPIX → Lightning; adding the reverse CLI
uses the separately tested reverse runner and is a later integration step.

```sh
./scripts/test-regtest.sh
CROSS_NETWORK_MARKET_MAKER_EXIT_QUALIFIED=1 ./scripts/test-exit.sh
```

The native exit test deliberately takes the receiving Lightning peer offline
and force-closes the channel. Its proof must follow the exact HTLC commitment,
confirmed timeout spend, CSV maturity, and final owned outputs. A final wallet
balance or an unrelated UTXO is insufficient exit evidence. Run this test after
all normal swaps because it consumes the test channel.

For another round, run bootstrap again to restore an active test channel when
needed. Stop only this repository's stack when done:

```sh
./scripts/stop-regtest.sh
```

## Capture your own CLI evidence

The recorder executes the supplied command and keeps its exact output, status,
timestamp, and SHA-256 digest. It never reads your environment into the report.
Use only commands that print public evidence; do not capture key files, raw
macaroons, invoice secrets, or unredacted node dumps.

```sh
python3 scripts/evidence_capture.py \
  --name local-protocol-tests --title "Rust protocol checks" \
  -- cargo test --locked
python3 scripts/evidence_render.py evidence/local-protocol-tests.json
```

Image rendering needs Pillow and a Menlo or DejaVu Sans Mono font. This is an
optional evidence dependency; the Rust market maker does not depend on Python
for protocol validation or settlement. The PNG is a faithful rendering of a
recorded transcript, not a desktop screenshot. A hash proves file consistency,
not independent chain verification; inspect the public transaction evidence
and rerun the commands to reproduce the result.

## Operate beyond this POC

Follow the [implementation plan](implementationPlan.md) in order: separate maker
wallets and custody first, then qualify each additional route, then add discovery
and market pricing. Keep the taker's local contract derivation, quote validation,
and recovery package independent from whichever relay or maker it selected.
Never advertise a route as atomic or unilaterally recoverable solely because it
has an adapter trait or a quote endpoint.
