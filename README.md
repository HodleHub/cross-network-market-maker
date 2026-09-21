# Cross-network market maker

Native Rust research POC for signed maker quotes, durable swap execution, and
HTLC recovery across Bitcoin-related networks. The executable is `xmm`.

The qualified environment is **local regtest only**. TEST-DEPIX is a synthetic
Elements asset created by bootstrap. It is not production DEPIX and has no
redemption value. Mainnet and public test networks are disabled.

- [Implementation plan and all twenty directed routes](docs/implementationPlan.md)
- [Operator tutorial](docs/marketMakerTutorial.md)
- [CLI evidence and provenance](evidence/README.md)

## Start locally

Prerequisites: Rust 1.94.1 or compatible newer stable, Docker with Compose,
Python 3, and a POSIX shell. The supplied Elements image is pinned to Linux
ARM64; this is the qualification profile used on Apple Silicon. An x86 build
requires its own verified binary checksum and qualification.

```sh
cargo build --locked
./scripts/start-regtest.sh
./scripts/bootstrap-regtest.sh
./scripts/status-regtest.sh
./target/debug/xmm routes
```

The stack runs Bitcoin Core, Elements, and two LND nodes with dedicated volumes.
Only loopback API ports are published. It starts a new local chain and does not
download Bitcoin history. Docker images and the Rust compiler cache still
consume disk space. See the [storage plan](docs/implementationPlan.md#storage-and-operational-constraints)
before considering a remote or pruned public-network backend.

LND's generated regtest certificate is pinned byte for byte. This local trust
model replaces public-CA, hostname, and certificate-expiry validation with the
explicit pin; Rustls still verifies the TLS handshake signatures. A different
certificate is rejected. Do not reuse this local fixture policy as a general
public-network TLS configuration.

## Validate

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
./scripts/test-regtest.sh
CROSS_NETWORK_MARKET_MAKER_EXIT_QUALIFIED=1 ./scripts/test-exit.sh
```

Run real-node tests serially. Run the native exit test last: it intentionally
force-closes a test Lightning channel while its peer is offline. The live test
targets are explicitly ignored in the normal Cargo suite and selected by the
regtest wrappers; ignored tests are not successful settlement evidence.

## Scope

The implementation separates protocol validation, quote signing, SQLite
reservation/state storage, Bitcoin and explicit Elements transaction builders,
LND observations, encrypted recovery, and the CLI. Rust constructs and signs
transactions; it does not call a TypeScript settlement implementation.

The twenty-entry catalogue expresses the route design. Entries are enabled
only after adapter qualification. A script primitive alone does not qualify
every route using that chain. Bark, Arkade, Spark, and public test networks are
future adapter work described in the plan.

Two local maker processes have independent signing keys and reservation
ledgers. The regtest settlement harness can access both test participants and
uses fixed test rates. Independent wallets, participant-separated custody,
remote discovery, market pricing, and production liquidity management require
the next stages in the plan.

An unknown payment result remains recoverable. A timeout refund needs the
contract deadline and valid refund keys; it may cost network fees. Native
unilateral exit is tested separately from swap refund. No test here establishes
production safety or removes Liquid's federation and asset-issuer assumptions.

## Stop and preserve recovery

```sh
./scripts/stop-regtest.sh
```

This preserves the dedicated Docker volumes and ignored `runtime/` directory.
Keep runtime recovery keys and node backups private. Deleting either can make
an unfinished swap unrecoverable. The reset script is an explicit destructive
test-fixture operation and is not part of the normal tutorial.

Images under `evidence/` are rendered from actual CLI transcripts with hashes,
timestamps, and command exit status. They are not generated illustrations or
desktop screenshots. Full text is retained beside each image.
