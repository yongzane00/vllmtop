# vllmtop

A colorful, DGXTOP-style terminal dashboard for monitoring [vLLM](https://docs.vllm.ai/)
servers — models, request activity, throughput, latency percentiles, and
KV-cache utilization — for one local instance or a whole fleet, from a single
static binary.

```
vllmtop            # monitors http://127.0.0.1:8000
```

**Fleet overview** — every endpoint plus rolling history charts in one view:

![Fleet view](docs/img/fleet.svg)

**Endpoint detail** — activity, cache, and latency percentiles for one server:

![Endpoint view](docs/img/endpoint.svg)

*(Screenshots captured against local mock vLLM servers.)*

## What it is

- **Read-only**: polls vLLM's HTTP endpoints (`/metrics`, and optionally
  `/health`, `/version`, `/v1/models`) with GET requests. It never proxies,
  inspects, or modifies inference traffic, and never controls the server.
- **Accelerator-agnostic**: works at the metrics layer, so NVIDIA, AMD,
  Intel, CPU, or any other vLLM backend all look the same. No `nvidia-smi`,
  no NVML/ROCm, no vendor libraries.
- **Self-contained**: one static Linux binary. No Python, Node, Docker,
  Prometheus, Grafana, browser, or system OpenSSL required. TLS via rustls.
- **Version-tolerant**: capabilities are detected from what the endpoint
  actually exposes. Missing metrics show as `--`; renamed metrics are mapped
  through an alias table; unknown or backend-specific metrics are parsed and
  tolerated (curation simply ignores them).

**What it deliberately does not do** (v1): per-user/conversation
attribution, GPU/CPU/host hardware monitoring, alerts or webhooks, web UI,
OpenTelemetry, server control, network discovery, telemetry. (Per-request
visibility exists only via the opt-in local [log tailer](#requests-pane-log-tailing);
vllmtop never proxies or inspects inference traffic.)

## Install

### From source

Requires stable Rust (1.88+): <https://rustup.rs>

```bash
git clone https://github.com/yongzane00/vllmtop && cd vllmtop
cargo build --release
./target/release/vllmtop --help
```

`cargo install --path .` also works.

Binary releases are planned: archives for `x86_64-unknown-linux-musl` and
`aarch64-unknown-linux-musl` with SHA-256 checksums, plus a
checksum-verifying installer script (`scripts/install.sh`) that installs to
`~/.local/bin` without root.

## Usage

```bash
# One local server (default)
vllmtop

# Several servers, named
vllmtop \
  --endpoint local=http://127.0.0.1:8000 \
  --endpoint spark-a=http://10.0.0.21:8000 \
  --endpoint spark-b=https://10.0.0.22:8000

# Bare URLs get a stable host:port name
vllmtop -e http://10.0.0.21:8000

# @CAP declares the server's --max-num-seqs (vLLM doesn't export it), so
# the endpoint view can draw running as an n/CAP bar
vllmtop -e dev=http://10.0.0.21:8000@8

# Recording is ON by default (~/.local/share/vllmtop/usage.db) — it feeds
# the fleet daily-usage charts. Custom path / retention / opt-out:
vllmtop --record ~/vllm-history.db --retention-days 14
vllmtop --no-record

# Slower refresh, more in-memory history
# (history is also capped at 4096 points per series, so at a 1s refresh the
#  effective in-memory window tops out around 68 minutes)
vllmtop --refresh-interval-ms 2000 --history-seconds 900

# Shell completions
vllmtop --completions bash > ~/.local/share/bash-completion/completions/vllmtop
```

### Configuration file

`~/.config/vllmtop/config.toml` (or `--config PATH`); see
[examples/config.toml](examples/config.toml) for the full annotated format.

Precedence: **defaults < config file < CLI flags**. A `--endpoint` flag
replaces the file's endpoint list entirely (no merging).

Authentication uses environment-variable *references* so secrets never live
in the file:

```toml
[[endpoints]]
name = "spark-a"
url  = "https://10.0.0.21:8443"
bearer_token_env = "SPARK_A_VLLM_TOKEN"   # name of the variable, not the token

[endpoints.header_env]
"X-Custom-Auth" = "SPARK_A_CUSTOM_AUTH"
```

Resolved secrets exist only inside the HTTP client; they are never logged,
recorded, displayed, or included in error messages. Displayed URLs are
redacted (no userinfo or query strings).

### Keys

| Key | Action |
| --- | --- |
| `q` / `Ctrl+C` | quit |
| `Tab` / `Shift+Tab` | next / previous view |
| `1` | fleet overview (endpoints + history charts) |
| `2`…`9` | endpoint tabs (more endpoints: keep pressing `Tab`) |
| `j`/`k`, arrows | select endpoint row (fleet view) |
| `Enter` | open the selected endpoint (fleet view) |
| `PgUp`/`PgDn`, mouse wheel | scroll the history charts |
| `g` / `G` | jump to top / last row |
| `s` | cycle fleet sort column |
| `t` | endpoint view: toggle charts+requests / tables |
| `r` | force refresh now (also re-queries daily usage) |
| `p` | pause display refresh (collection continues) |
| `+` / `-` | faster / slower refresh |
| `?` | help |

### Views

- **Fleet (1)** — everything at a glance, three stacked sections. Top: a
  dense per-endpoint table (health/staleness, model, running/waiting, KV
  bar, prompt/generation throughput, completion rate, worst TTFT p95, new
  errors/preemptions, data age). Middle: **past-30-days usage bar charts**
  — output tokens/day, input tokens/day, requests/day, summed across the
  fleet per local calendar day, read from the recorded history (see
  [Recording](#recording)); bars size dynamically with the terminal, and
  days without observations show `--`, never a fabricated zero. Bottom: a
  scrollable grid of rolling history charts (default 5 min at 1 s
  resolution) for running/waiting, KV usage, throughputs, completion rate,
  latency p95s, errors, and preemptions — multiple endpoints overlay as
  separate colored lines. Histogram data is never merged across endpoints
  unless bucket boundaries match.
- **Endpoint (2…N)** — one server in depth. A pulse strip answers the first
  question at a glance: an animated `GENERATING` indicator while requests
  are running, generation/prompt tokens/s with session peaks (`pk`),
  running (as an `n/max` bar when the endpoint's `max_running` — or the
  `@CAP` endpoint suffix — mirrors the server's `--max-num-seqs`, which
  vLLM doesn't export), waiting, lifetime completions served and a token
  odometer (`tokens 5.4M in / 1.2M out` — prompt tokens consumed / tokens
  generated since the server started), and a full-width KV-cache bar with
  absolute tokens that visibly grows while a conversation generates.
  Below it, by default: **prefill and generation token-rate charts** (live
  rate in each header) on the left, and the **requests pane** (see below)
  on the right. Press `t` for the classic tables: cache hit rates, finish
  reasons, errors/aborts, preemptions, the latency percentile table (TTFT,
  inter-token, e2e, queue, prefill, decode; p50/p95/p99/mean over a rolling
  window) plus tokens-per-request distributions, and trend charts.
  Multi-engine (data-parallel) or multi-model servers keep separate
  per-series rows, including per-model lifetime token totals.

### Requests pane (log tailing)

vLLM's HTTP API exposes no per-request information, so the requests pane
(age, prompt preview, request id, max_tokens) works by **tailing the vLLM
server's stdout log** — opt-in, per endpoint, local files only:

```toml
[[endpoints]]
name = "local"
url  = "http://127.0.0.1:8000"
log_file = "/var/log/vllm/server.log"   # tail -f semantics: new requests only
```

Requirements on the vLLM side: start the server with
`--enable-log-requests` (request ids + params appear at INFO); prompt
previews additionally need `VLLM_LOGGING_LEVEL=DEBUG`, otherwise the prompt
column shows `--`.

Privacy guarantees: prompt previews are truncated to 120 bytes, kept only
in a bounded in-memory ring (200 entries), and are **never** written to the
SQLite recorder, tracing logs, or any file. Tailing starts at the end of
the file (like `tail -f`), reads are bounded per tick, and rotation and
truncation are handled. vllmtop still never proxies, inspects, or modifies
inference traffic.

### Data semantics worth knowing

- **`--` means unavailable** — the endpoint did not expose that metric.
  `0` always means a real zero.
- **Rates** (tokens/s, completions/s) are computed from counter deltas over
  a monotonic clock. The first sample after startup or a server restart
  shows `--` rather than a wrong number; counter resets are detected and
  flagged with a `RESTARTED` badge instead of producing negative rates.
- **Latency percentiles are estimates** from Prometheus histogram bucket
  deltas over a rolling window (default 60 s, `--percentile-window-seconds`),
  linearly interpolated within buckets. When a percentile lands in the
  top `+Inf` bucket the estimate saturates at the highest finite bound.
- **Staleness**: an endpoint with no successful scrape for ~3 intervals
  shows `STALE` and its last good snapshot stays visible (values freeze
  rather than blank out). Failed endpoints retry with capped exponential
  backoff and jitter; they never block healthy endpoints.
- **Optional endpoints degrade to unknown**: a server without `/health`
  (404/405) shows health as unknown, not unhealthy; `/version` and
  `/v1/models` absence shows `--`. Only 5xx from `/health` marks an
  endpoint unhealthy.
- **Endpoint URLs are bases**: probe paths (`/metrics`, …) are appended to
  the configured URL's path. Query strings, fragments, and userinfo are
  stripped at startup — credentials belong in `bearer_token_env` /
  `header_env`, never in the URL.
- **Pause (`p`)** freezes automatic display refresh only. Collection, rate
  math, history, and recording all continue, so unpausing never corrupts
  rates.

### Recording

Recording is **on by default**: aggregate samples (endpoint, model, engine,
metric id, value, wall timestamp) append to a SQLite database in WAL mode at
`$XDG_DATA_HOME/vllmtop/usage.db` (falling back to
`~/.local/share/vllmtop/usage.db`). `--record PATH` (or `record_path` in the
config file) changes the location; `--no-record` (or `no_record = true`)
turns it off — which also empties the fleet daily-usage charts, since they
are computed from this database (cumulative token/request counters are
snapshotted every 30 s and aggregated into per-local-day totals with
restart-aware positive deltas). Never prompts, request bodies, tokens, or
headers. Retention defaults to 30 days (`--retention-days`,
`retention_days`), cleaned up in bounded batches. Writes happen on a
dedicated thread; if the database stalls, batches are dropped and counted
(shown in the header) rather than ever blocking collection. The header's
`rec:` counter reports **verified** written rows.

Schema (`meta` + `samples`) is documented in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#recording-schema); the schema
version is stamped in the database and newer-versioned databases are refused
rather than corrupted.

### Terminal support

256-color and truecolor terminals are detected automatically;
[`NO_COLOR`](https://no-color.org/) or `--no-color` switches to a monochrome
theme (bold/dim only, bars drawn in ASCII). Resizes are handled live; the
layout is tuned for ~120×30 but degrades gracefully below that. The terminal
is restored on quit, Ctrl+C, SIGTERM, and panics.

## Compatibility

vLLM ≥ 0.8 era metric names (V1 engine) are the primary target, with alias
mapping for older spellings (e.g. `vllm:gpu_cache_usage_perc`,
`vllm:time_per_output_token_seconds`). Anything unrecognized is parsed and
skipped without breaking curation. See [docs/METRICS.md](docs/METRICS.md)
for the exact table of curated metrics, aliases, and behavior when they are
missing.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release
```

On Ubuntu/WSL2 you need only `build-essential` and rustup's stable
toolchain; SQLite is bundled, TLS is rustls. See
[CONTRIBUTING.md](CONTRIBUTING.md) and the
[documentation index](docs/README.md) — architecture, metric semantics,
and design deep-dives all live under `docs/`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
