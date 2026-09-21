# Development contract

- This is a standalone Rust regtest research POC, independent of Hodler.
- Rust owns the protocol, signatures, durable state, adapters, transaction construction, and CLI; do not shell out to TypeScript/JavaScript for these operations.
- Use idiomatic Rust, typed structures, small modules and `Result` errors. Document public APIs. No unsafe code. Avoid panics and unchecked amounts in production paths.
- Only Bitcoin regtest, Elements regtest with locally issued TEST-DEPIX, and LND regtest are enabled. Reject mainnet, public testnet, unknown networks and unqualified Ark/Bark/Arkade/Spark routes.
- Real tests must use actual nodes; do not report simulations as settled swaps or native exits.
- Never commit runtime seeds, private keys, preimages, macaroons, TLS private material, real credentials, runtime databases or chain data. Public transaction IDs and sanitized command transcripts are allowed. Clearly synthetic deterministic unit-test fixtures are allowed; live tests generate fresh secret material.
- Keep generated runtime files under ignored runtime/ with restrictive permissions and durable recovery outboxes.
- Preserve the original TS POC as read-only reference; do not edit it.
- All code and repository documentation are English. User-facing updates may be Portuguese.
- Before publication run cargo fmt --check, cargo clippy --all-targets -- -D warnings, cargo test, serial real-node tests, and secret/diff checks.
- CLI image evidence must be captured from actual Rust command output. Never fabricate screenshots or use image generation for evidence.
