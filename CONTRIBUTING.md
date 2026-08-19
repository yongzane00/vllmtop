# Contributing to vllmtop

Thanks for your interest! The project is young; the most valuable
contributions right now are metric-compatibility reports from different vLLM
versions/backends and fixtures for them.

> **License note:** the project license is not yet decided. Until a LICENSE
> file exists, contributions cannot be merged from third parties — watch the
> README's Project status section.

## Development setup (Ubuntu / WSL2)

```bash
# toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -c rustfmt -c clippy
sudo apt-get install -y build-essential   # C toolchain for bundled SQLite

git clone <repo-url> && cd vllmtop
cargo test
cargo run -- --help
```

No system SQLite or OpenSSL needed (bundled SQLite, rustls TLS). MSRV is
declared in `Cargo.toml` (`rust-version`); CI builds with current stable.

Tip for WSL2: keep the checkout on the Linux filesystem, or set
`CARGO_TARGET_DIR` to a Linux-side path when the checkout lives under
`/mnt/c`, otherwise builds are slow.

## Before sending a change

All four must pass — CI enforces them:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release
```

## Testing conventions

- Core data logic (parser, rates, histograms, config, storage) is developed
  test-first; add the failing test with your fix.
- Real-world `/metrics` output goes in `tests/fixtures/` — **sanitized**:
  no private hostnames, IPs, org names, or model deployments you cannot
  publish. Replace model names with `example-org/...`.
- Unit and integration tests must not touch the network; use `httpmock`
  (see `tests/integration.rs`).
- To try the TUI without a GPU or vLLM install, run the mock server test
  fixture through any static file server, or point vllmtop at any live vLLM
  endpoint you own (it only issues GETs).

## Code conventions

- `#![forbid(unsafe_code)]` stays.
- No panics on network/config/parse/render/storage paths: return errors or
  degrade to `--`/N/A. `unwrap` is acceptable only where an invariant makes
  failure impossible (document it) and in tests.
- Zero must never be conflated with unavailable — carry `Option` all the way.
- Anything that buffers must be bounded (channel, ring, map with eviction).
- Metric name strings live only in `src/metrics/normalize.rs`.
- Secrets: env-var names in config, values only inside HTTP clients. Never
  in state, logs, errors, or the recorder. New display code must go through
  the redaction helpers for URLs.

## Reporting metric incompatibilities

Open an issue with: vLLM version, hardware backend, engine flags, and a
sanitized copy of `/metrics` (`curl http://HOST:PORT/metrics`). That is
usually enough to add an alias or fixture within minutes.
