# vllmtop

A colorful, DGXTOP-style terminal dashboard for monitoring [vLLM](https://docs.vllm.ai/)
servers — models, request activity, throughput, latency percentiles, and
KV-cache utilization — for one local instance or a whole fleet, from a single
static binary.

```
vllmtop            # monitors http://127.0.0.1:8000
```

**Fleet overview** — endpoint table, past-30-days usage, rolling history:

![Fleet view](docs/img/fleet.svg)

**Endpoint detail** — pulse strip, token-rate charts, live requests pane
(press `t` for latency-percentile tables):

![Endpoint view](docs/img/endpoint.svg)

*(Screenshots captured against local mock vLLM servers.)*

## What it is

- **Read-only**: polls vLLM's HTTP endpoints (`/metrics`, and optionally
  `/health`, `/version`, `/v1/models`) with GET requests. Never proxies,
  inspects, or modifies inference traffic; never controls the server.
- **Accelerator-agnostic**: works at the metrics layer — NVIDIA, AMD,
  Intel, CPU, any vLLM backend. No `nvidia-smi`, NVML, or vendor libraries.
- **Self-contained**: one static Linux binary. No Python, Docker,
  Prometheus, Grafana, or system OpenSSL. TLS via rustls.
- **Version-tolerant**: capabilities are detected from what the endpoint
  exposes; renamed metrics map through an alias table; missing metrics show
  `--` (never a fake 0); unknown metrics are tolerated.
- **Honest numbers**: rates are reset-aware (server restarts show a
  `RESTARTED` badge, never negative rates), percentiles are labelled
  estimates, stale endpoints show `STALE`, and percentages are never
  averaged unless capacity-weighted.

**What it deliberately does not do**: per-user/conversation attribution,
host hardware monitoring, alerts/webhooks, web UI, OpenTelemetry, server
control, telemetry. Per-request visibility exists only via the opt-in local
[log tailer](#requests-pane).

## Install

Requires stable Rust (1.88+): <https://rustup.rs>

```bash
git clone https://github.com/yongzane00/vllmtop && cd vllmtop
cargo build --release
./target/release/vllmtop --help
```

`cargo install --path .` also works. Binary releases (musl x86_64 +
aarch64, SHA-256-verified installer) are planned.

> Interested in seeing vllmtop on crates.io so `cargo install vllmtop`
> just works? Issues and pull requests are welcome.

## Usage

```bash
vllmtop                                        # one local server
vllmtop -e local=http://127.0.0.1:8000 \
        -e spark-a=https://10.0.0.21:8443      # a named fleet
vllmtop -e dev=http://10.0.0.21:8000@8         # @8 = server's --max-num-seqs
vllmtop --refresh-interval-ms 2000             # slower refresh
vllmtop --no-record                            # disable usage recording
vllmtop --completions bash                     # shell completions
```

Recording is on by default (`~/.local/share/vllmtop/usage.db`, 30-day
retention) and feeds the fleet's daily-usage charts; `--record PATH`
relocates it. Only aggregate metric samples are stored — never prompts,
tokens, or headers.

### Configuration file

`~/.config/vllmtop/config.toml` (or `--config PATH`); see
[examples/config.toml](examples/config.toml) for the annotated format.
Precedence: defaults < file < CLI flags. Secrets are environment-variable
*references* — never values in the file, never logged or displayed:

```toml
[[endpoints]]
name = "spark-a"
url  = "https://10.0.0.21:8443"
bearer_token_env = "SPARK_A_VLLM_TOKEN"   # name of the variable, not the token
max_running = 8                           # server's --max-num-seqs
log_file = "/var/log/vllm/server.log"     # enables the requests pane
```

### Keys

| Key | Action |
| --- | --- |
| `q` / `Ctrl+C` | quit |
| `Tab` / `1`…`9` | switch views |
| `j`/`k`, `Enter` | select / open endpoint (fleet view) |
| `PgUp`/`PgDn`, wheel | scroll the history charts |
| `s` | cycle fleet sort column |
| `t` | endpoint view: charts+requests ⇄ tables |
| `r` | force refresh |
| `p` | pause display (collection continues) |
| `+` / `-` | faster / slower refresh |
| `?` | help |

### Requests pane

vLLM's HTTP API exposes no per-request data, so the requests pane (age,
prompt preview, request id, max_tokens) tails the **server's own stdout
log** — opt-in per endpoint via `log_file`, local files only. The server
needs `--enable-log-requests`; prompt previews additionally need
`VLLM_LOGGING_LEVEL=DEBUG`. Previews are bounded and in-memory only —
never recorded or written anywhere. Details:
[docs/LOG-TAILING.md](docs/LOG-TAILING.md).

## Compatibility

vLLM ≥ 0.8 metric names (V1 engine) are the primary target, with aliases
for older spellings. Unrecognized metrics are parsed and skipped without
breaking anything. The exact curated-metric table:
[docs/METRICS.md](docs/METRICS.md).

Works on 256-color and truecolor terminals; `NO_COLOR` / `--no-color`
gives a monochrome ASCII theme. Tuned for ~120×30, degrades gracefully
below.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and the
[documentation index](docs/README.md) — architecture, metric semantics,
and design deep-dives live under `docs/`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
