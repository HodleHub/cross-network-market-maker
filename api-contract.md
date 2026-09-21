# Current POC protocol notes

This document describes the implemented interfaces. The broader roadmap is in
[the implementation plan](docs/implementationPlan.md). A listed route is not
qualified merely because its types can be serialized.

## Participant boundary

The Rust CLI coordinates a local regtest harness. Two maker HTTP processes can
use different signing keys, fee offers, and SQLite ledgers. Settlement currently
has local access to the selected ledger and both test participants' nodes and
recovery material. Independent remote settlement and participant-separated
custody are future work.

Intents are client-created requests; the current protocol does **not** implement
client-signed intents. Makers sign their quotes with secp256k1 ECDSA. Clients
verify the signatures against a separately pinned key-ID registry. Neither a
response's display name nor a discovery address is a trusted signing key.

## Intent and quote binding

`core::rfq::{Intent, Quote, SignedQuote}` carry protocol version, endpoint and
asset identities, exact input/minimum output amounts, fees, the SHA-256 hash
commitment, public keys, destinations, nonce, expiry, and deadline context.
A Lightning quote additionally binds the exact BOLT11 request. Amounts are
canonical decimal strings; heights and timestamps are typed unsigned integers.

Quote signatures use a versioned, length-prefixed field encoding, independent
of JSON formatting. Quote validation checks the intended route and asset hash,
amounts, fee bounds, nonce, adapter, client terms, expiry, and pinned key. The
CLI also validates the BOLT11 network, amount, and payment hash.

Preparation observes local node heights and creates a deadline context. The
runner separately verifies actual node state and cross-network timing before
new payment exposure. Heights from Bitcoin and Elements are never compared as
if they used a common clock. The conservative regtest block-time policy is
not a production confirmation or liveness guarantee.

The initial RFQ fixes the input lot and Lightning invoice amount. Makers can
compete on the signed fee field. This is not a live FX price engine or a
separate maker-fee payout implementation.

## Durable acceptance and effects

Each SQLite store uses WAL, `synchronous=FULL`, and checked integer accounting.
A maker reserves destination inventory. Quote, hash, and reservation identities
prevent a conflicting reservation from silently consuming the same capacity.
An attached reservation remains locked when its client's state is unknown.

The client and maker use **separate database transactions**. There is no atomic
transaction spanning both databases. Recovery reconciles the two stores
idempotently against verified payment observations. Tests exercise the crash
window where the client records output settlement before the maker ledger is
consumed. A repeated callback must complete that consumption exactly once.

The normal forward states are:

```text
CREATED -> INPUT_FUNDING -> INPUT_LOCKED -> OUTPUT_PENDING
                                      -> OUTPUT_UNKNOWN (when necessary)
                                      -> OUTPUT_SETTLED -> CLAIM_PENDING -> SETTLED
```

An unknown payment result is not a failed payment. Once a payment is verified,
recovery completes the claim without starting a replacement payment. Fresh
acceptance enforces quote expiry; an already accepted immutable session can be
reconciled after that expiry.

Encrypted recovery contains private keys, the preimage, asset/amount/deadline
terms, and the derived contract. A signed transaction outbox is persisted before
broadcast. Transaction IDs and the same Lightning payment hash are reused on
replay. Public evidence omits private material.

## Node adapters

`RpcClient` permits authenticated loopback HTTP(S) endpoints and disallows
embedded URL credentials, query strings, fragments, and redirects. Chain
observers and broadcasters check the actual node network.

Bitcoin and Elements builders use the Rust `bitcoin` and `elements` crates.
They construct hash/time-locked scripts and sign funding/claim/refund spends.
The Elements POC uses explicit assets and values, with separate policy-asset
fee inputs and explicit fee outputs. It does not implement confidential HTLC
funding. LBTC-as-payment primitives require distinct payment and fee outpoints.

`LndRestClient` accepts local HTTPS with an exact certificate pin and standard
TLS handshake signature checks. It verifies the actual Bitcoin regtest network,
BOLT11 requests, payment hash and amount, accepted HTLC information, and terminal
payment observations. Only a successful payment with a matching SHA-256
preimage exposes that preimage to the caller. Transport interruption is
recoverable uncertainty.

`SwapChainAdapter` and the Lightning adapter are composed by
`run_forward_swap` and `run_reverse_swap`. `SwapObserver` persists phases around
external effects. The concrete regtest chain adapter mines confirmation blocks
as part of the test harness; it is not a public-network confirmation service.

## Refund and native exit

The high-level failed-swap refund path observes a terminal failed payment and a
canceled invoice before using the refund branch. Raw chain HTLC refund
primitives remain separate. A refund needs its absolute deadline and available
fee inputs. It is not a reversal of a successful Lightning payment.

Native Lightning exit has separate parsing and proof types under `src/exit`.
Its real-node qualification must identify the exact hash-linked HTLC timeout,
follow the transaction's matching output, verify CSV sequence and confirmation
maturity, and account for final owned outputs and fees. Ark/Bark/Arkade/Spark
native exits are not enabled by this Lightning proof.

The witness and output checks follow the
[BOLT 3 transaction formats](https://github.com/lightning/bolts/blob/master/03-transactions.md#htlc-timeout-and-htlc-success-transactions):
the offered-HTLC hash is embedded in its witness script, while the timeout
transaction creates a different delayed output. These scripts must not be
treated as interchangeable. Anchor timeout signatures bind corresponding input
and output indices, which matters when LND batches the transaction.

## Qualification

Run normal protocol tests with `cargo test --locked`. Real-node targets are
ignored by default and must be selected explicitly with `--ignored` against the
isolated stack. `scripts/test-regtest.sh` selects the regular live targets;
`scripts/test-exit.sh` selects only native exit and requires its opt-in. The
[evidence directory](evidence/README.md) records actual command output and its
provenance. Never count an ignored test as a passed live qualification.
