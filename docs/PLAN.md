# vllmtop — Implementation Plan

Status: living document. Milestones are checked off as they are completed and verified.

## What is being built

A single-binary Linux TUI (`vllmtop`) that polls one or more vLLM servers over
read-only HTTP (`/metrics`, plus optional `/health`, `/version`, `/v1/models`)
and renders fleet + per-endpoint serving telemetry in a dense, DGXTOP-style
terminal UI. No proxying, no inference traffic, no accelerator APIs, no
per-user/conversation attribution. Per-REQUEST visibility exists only via an
opt-in local log tailer (owner-approved scope change 2026-08; see key
decision 9).

## Architecture (message passing, no shared mutable state)

```
per-endpoint collector task (tokio)          crossterm EventStream
  poll loop: timeout + backoff + jitter              |
  GET /metrics (+health/version/models)              v
  parse -> RawScrape ----bounded mpsc----> main select! loop (owns AppState)
                                             |  reduce: normalize -> rates ->
                                             |  histogram windows -> ring history
                                             |--bounded channel--> SQLite recorder
                                             |                     (dedicated thread)
                                             v
                                       Ratatui render (tick-driven)
```

- Collectors never touch shared state; they send `AppEvent::Scrape` messages.
- The main loop is the single owner of `AppState`; rendering borrows it.
- The optional recorder runs on its own OS thread; a slow/failed DB drops
  batches (with a counter) instead of blocking collection.

## Module map

```
src/
├── main.rs          entry; terminal guard (panic-safe restore); runtime
├── cli.rs           clap arg definitions
├── config.rs        TOML schema, precedence merge, validation
├── app.rs           select! loop, event dispatch, key handling, view routing
├── event.rs         AppEvent / input mapping
├── collector/       reqwest client (rustls), per-endpoint poll task
├── metrics/         parse.rs (own Prometheus text parser), model.rs (types),
│                    normalize.rs (capability/alias layer), rates.rs (reset-aware
│                    counter deltas), histogram.rs (windowed percentile estimates)
├── logtail/         opt-in per-endpoint tailer of vLLM's stdout log (bounded,
│                    std-only line parser) feeding the requests pane
├── state/           AppState, EndpointState, ring-buffer history,
│                    bounded per-request ring (requests.rs)
├── ui/              theme (NO_COLOR aware), fleet view (endpoint table +
│                    daily-usage bar charts + history-chart grid), endpoint
│                    detail view (rate charts + requests pane; 't' = tables),
│                    help overlay, formatting widgets
└── storage/         SQLite schema, batched writes, retention; usage.rs =
                     read-only per-local-day aggregation for the fleet charts
```

## Key decisions

1. **Own Prometheus text parser** (std-only, table-tested). The live vLLM 0.24
   capture shows histograms whose *family name* ends in `_total`
   (`vllm:iteration_tokens_total`), summaries without quantiles, `+Inf`
   buckets, colons in names, and paired `_created` gauges. Type must come from
   `# TYPE`, so a small correct parser beats a loosely-maintained dependency.
   The parser lives behind `metrics::parse` so it can be swapped.
2. **Capability layer, not version checks**: `normalize.rs` maps curated
   metrics through an alias table (e.g. `vllm:kv_cache_usage_perc` with
   `vllm:gpu_cache_usage_perc` as a legacy alias) and reports what it found;
   everything else is parsed and tolerated (curation ignores it).
3. **Rates**: per-(endpoint, metric, full label set) tracker keyed on monotonic
   `Instant`. Negative delta ⇒ counter reset ⇒ rate = unavailable for that
   interval, never negative. First sample after start/reset ⇒ unavailable.
4. **Histogram percentiles**: ring of cumulative bucket snapshots; p50/p95/p99
   estimated by linear interpolation over the *delta* between now and the
   snapshot ~window ago (default 60 s), labelled as estimates. Bucket-layout
   changes or negative deltas invalidate the window.
5. **Fleet KV aggregate**: capacity-weighted using
   `cache_config_info.kv_cache_size_tokens` when every healthy endpoint
   exposes it; otherwise per-endpoint bars plus a clearly labelled unweighted
   mean. Histogram buckets are only merged across endpoints when boundaries
   are identical.
6. **Zero vs unavailable**: every curated value is `Option<_>` end to end;
   missing renders as `--`, never `0`.
7. **Secrets**: TOML stores env-var *names*; values are resolved at client
   build time, live only in reqwest header maps, and are never Debug-printed,
   logged, or persisted. Displayed URLs are redacted (no userinfo/query).
8. **Keybindings**: `q` quit, `Tab`/`Shift+Tab` cycle views, `1`..`9` direct
   tabs (1 = fleet), `j`/`k` select row, `Enter` open endpoint, `PgUp`/`PgDn`
   scroll the embedded charts, `r` force refresh, `p` pause rendering
   (collection continues), `s` sort, `+`/`-` refresh interval, `?` help.
9. **View consolidation (owner decision, post-M11)**: the standalone History
   view was merged into the fleet tab (chart grid below the endpoint table)
   and the Raw Metrics view was removed, along with per-endpoint retention of
   the parsed raw scrape.
10. **Usage accounting (owner decision, 2026-08)**: recording is ON by
    default (`$XDG_DATA_HOME/vllmtop/usage.db`; `--no-record` opts out);
    cumulative token/request counters are snapshotted every 30 s and the
    fleet tab renders past-30-days bar charts from segment-wise positive
    deltas bucketed by local day in SQL (`storage/usage.rs`) — never by
    integrating rate samples. "Images/day" was considered and dropped: vLLM
    exports no image-count metric (`vllm:mm_cache_queries_total` is a cache
    proxy, not a count).
11. **Per-request pane via log tailing (owner decision, 2026-08 — reverses
    the original "no per-request visibility" exclusion)**: opt-in
    per-endpoint `log_file` tails the server's stdout (poll-based, bounded,
    rotation-aware, starts at EOF) and parses `RequestLogger` lines for id /
    max_tokens / prompt preview (previews only when the server logs at
    DEBUG). Previews are bounded (120 bytes, 200-entry ring), in-memory
    only, and never reach the recorder or any log/file. The endpoint view
    defaults to rate charts + requests pane; `t` restores the tables.

## Milestones

- [x] M0 Environment: WSL Ubuntu toolchain (Rust 1.97.1), live vLLM 0.24.0
      metrics captured for fixture design
- [x] M1 Vertical slice: cargo init, CLI/config, one collector, parser,
      minimal state, minimal fleet view rendering live data
- [x] M2 Metrics layer: normalization/aliases, rates + reset handling,
      histogram windows + percentiles, ring history (unit-tested)
- [x] M3 Multi-endpoint: concurrent collectors, failure isolation, dynamic
      tabs, staleness display
- [x] M4 Views: endpoint detail, History, Raw metrics (search/sort)
- [x] M5 Auth/TLS: HTTPS (rustls), bearer/header env refs, redaction
- [x] M6 Recording: SQLite (WAL, batched, dedicated thread), retention
- [x] M7 Hardening: NO_COLOR, resize, small terminals, panic-safe restore,
      bounded everything
- [x] M8 Tests: parser/rates/histogram/config unit suites, httpmock
      integration test (collect → normalize → state → recover)
- [x] M9 Docs: README, CONTRIBUTING, ARCHITECTURE, METRICS compat,
      example config
- [x] M10 CI/release: GitHub Actions (fmt, clippy -D warnings, test, build),
      x86_64 + aarch64 release workflow, installer scaffold (repo URL
      parameterized — release blocker until decided; nothing published)
- [x] M11 Verification: fmt/clippy/test/release-build clean; mock + live
      read-only validation; failure-injection run (live endpoint + dead
      endpoint), NO_COLOR + 60×12 terminal runs, SIGTERM restore verified

## Deliberately open

- **crates.io publish: decide at release time** (`publish = false` until
  then; the crate name `vllmtop` was still free as of 2026-08-25).

## Decided

- **License: MIT OR Apache-2.0** (dual, Rust-ecosystem standard) — decided
  2026-08-21; LICENSE-MIT + LICENSE-APACHE are in the tree.
- **Repository: `github.com/yongzane00/vllmtop`** — resolved 2026-08-25
  from the owner's own remote; baked into Cargo.toml, README/CONTRIBUTING
  clone URLs, and the installer's default slug. Remaining release steps:
  a first tag to exercise the release workflow, and verifying the aarch64
  binary on real hardware.
