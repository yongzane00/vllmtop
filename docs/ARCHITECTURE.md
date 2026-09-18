# Architecture

vllmtop is built around ownership and message passing: exactly one task owns
the application state, everything else communicates over bounded channels.

```
┌────────────────────┐   AppEvent::Scrape    ┌──────────────────────────────┐
│ collector task #1  ├──────────────┐        │ main select! loop (app.rs)   │
├────────────────────┤              │        │  owns AppState               │
│ collector task #2  ├──────────────┼───────▶│  reduce: curate → rates →    │
├────────────────────┤   bounded    │        │  histogram windows → history │
│ …one per endpoint  ├──── mpsc ────┘        │  render (throttled, dirty-   │
└────────────────────┘                       │  flag driven)                │
┌────────────────────┐   AppEvent::Key       │            │                 │
│ input thread       ├──────────────────────▶│            ▼ bounded sync    │
│ (blocking reads)   │                       │  ┌──────────────────────┐    │
└────────────────────┘                       │  │ SQLite recorder      │    │
                                             │  │ (dedicated OS thread)│    │
                                             └──┴──────────────────────┴────┘
```

Design rules:

- **No shared mutable state.** Collectors never see `AppState`; the reducer
  never does I/O; the renderer borrows state immutably.
- **Everything is bounded**: the event channel (256), the recorder queue
  (64 batches, dropped+counted when full), history rings (time window plus a
  hard point cap), histogram snapshot rings, counter banks (generation-based
  eviction), and response body sizes (8 MiB metrics, 256 KiB metadata).
- **The render loop is not hot.** Draws happen on events, throttled to
  ~10 fps, with a 500 ms heartbeat tick so ages/staleness advance. Idle CPU
  is effectively zero.

## Module tour

| Module | Responsibility |
| --- | --- |
| `main.rs` | CLI → config → terminal guard → runtime → wiring. Restores the terminal on quit, SIGTERM, and panics (ratatui's panic hook). |
| `cli.rs` | clap definitions, shell completions. |
| `config.rs` | TOML schema, precedence merge (defaults < file < CLI), validation, duplicate-name rejection, env-reference secret resolution, URL redaction. |
| `collector/` | One tokio task per endpoint: `/metrics` + `/health` every cycle (concurrently), `/version` + `/v1/models` every 30th cycle, parsing off the UI path, per-request timeouts (3 s connect / 10 s total), capped exponential backoff with ±10 % jitter, no redirects. Auth headers live only inside the reqwest client. |
| `metrics/parse.rs` | Own Prometheus text parser (see below). |
| `metrics/model.rs` | `ScrapeText` / `MetricFamily` / `Sample` / `LabelSet` types. |
| `metrics/normalize.rs` | Curated extraction + capability detection + alias table, keyed by `(model_name, engine)`. The only file that knows `vllm:*` name strings. |
| `metrics/rates.rs` | Reset-aware counter deltas over a monotonic clock; `CounterBank` with generation eviction. |
| `metrics/histogram.rs` | Ring of cumulative snapshots → windowed bucket deltas → p50/p95/p99 interpolation. |
| `state/` | `EndpointState::apply(outcome)` reducer, freshness/staleness, endpoint aggregates (incl. KV capacity weighting), fleet roll-up (`aggregate_fleet`), ring-buffer history, bounded per-request ring (`state/requests.rs`), bounded observation feed (`state/events.rs`). |
| `logtail/` | Opt-in per-endpoint tailer of the vLLM server's stdout log (`log_file`): poll-based (500 ms), starts at EOF, rotation/truncation-aware, bounded (256 KiB/tick, 16 KiB/line, 256 events/batch), std-only line parser with ANSI stripping. Sends `AppEvent::RequestLog`. |
| `ui/` | Theme (truecolor/256/mono), formatting, the fleet view (card band + endpoint table + daily-usage bars + history-chart grid), and the endpoint detail view (card band + panel modes). Pure functions of `&App`. |
| `ui/cards.rs` | The stat-card widget both views share. Takes `&Theme`, not `&App`, so it is testable without an application. Its constructors are the single place `None` becomes `--`: an unavailable value gets no unit, no bar and no sparkline. |
| `ui/panels.rs` | The overview's detail band: latency percentiles (worst across series), server/model facts, and the observed-events feed. |
| `storage/` | SQLite recorder thread: WAL, batched transactions, chunked retention cleanup, verified write counters, schema versioning. On by default (`$XDG_DATA_HOME/vllmtop/usage.db`); `--no-record` opts out. |
| `storage/usage.rs` | Read side: per-local-day usage totals (tokens in/out, requests) from 30 s cumulative-counter snapshots, computed in SQL as segment-wise positive `LAG` deltas (restart-safe) on a second read-only WAL connection via `spawn_blocking`; result cached in `App::usage`. |

### Endpoint-global metrics, and what they do NOT mean

`series_key()` requires a `model_name` label, so unlabeled samples were
dropped by curation. `CuratedScrape::info` (`EndpointInfo`) now carries them,
with three labels the UI must never blur:

- `process_resident_memory_bytes` is the **HTTP front-end** process. In
  vLLM V1 the model runs in a separate engine process, so this is not model
  or KV memory; the info panel says `api proc mem` for exactly that reason.
  All `process_*` metrics vanish under `--api-server-count > 1` (multiprocess
  mode drops prometheus_client's default collector), so every field is
  `Option` and the card degrades to `--`.
- `cache_config_info.gpu_memory_utilization` is the fraction the operator
  **asked for**, not a measurement — rendered as `(configured)`.
- `cache_config_info.kv_cache_memory_bytes` is the literal string `"None"`,
  so KV is reported in **tokens**. Capacity comes from `kv_cache_size_tokens`
  directly and is never derived as `num_gpu_blocks × block_size`: on a hybrid
  Mamba model those disagree (442 × 784 = 346,528 vs a true 342,803).

Only a fixed whitelist of `cache_config_info` labels is kept, each truncated:
the label set is server-controlled, so copying it wholesale would be an
unbounded allocation driven by the monitored process.

### Log tailing and the privacy firewall

Prompt previews parsed from server logs are capped at 120 bytes, held only
in the 200-entry in-memory ring in `state/requests.rs`, and are never
`tracing`-logged or persisted. The recorder consumes exclusively
`EndpointState::current_samples()` and `cumulative_samples()`, neither of
which touches the request ring — that separation is the firewall keeping
request contents out of SQLite. Tailing is read-only `tail -f`: first open
seeks to the file's end, a shrunken file restarts from offset 0, and a
rotated-in file *longer* than the previous offset is the one undetectable
case (documented; worst case one tick's lines are skipped).

## Why an own Prometheus parser

The live vLLM 0.24 capture (sanitized in `tests/fixtures/`) exposes several
traps: a **histogram family whose name ends in `_total`**
(`vllm:iteration_tokens_total` — type must come from `# TYPE`, not name
suffixes), summaries without quantiles, colons in metric names, `+Inf`
bounds, paired `_created` gauges, and escaped label values. The maintained
parser crates were last released in 2023; rather than depend on an
unmaintained crate, ~300 table-tested lines in `metrics/parse.rs` implement
the text format with the same leniencies as Prometheus's own scrape-time
parser (blanks between tokens, trailing label commas, out-of-order families,
unknown-escape pass-through, `# EOF` tolerated). Invalid lines are skipped
and reported per line, never failing a whole scrape. Everything downstream
sees only `ScrapeText`, so the parser is swappable.

## Correctness decisions

- **Zero vs unavailable**: every curated value is `Option<f64>` from
  normalization to rendering. `--` in the UI is `None`, never `0`.
- **Rates**: `rate = Δcounter / Δmonotonic-time` per full label set. A
  negative delta ⇒ counter reset ⇒ that interval reports `Reset` (rate
  unavailable, `RESTARTED` badge) and the tracker restarts. First samples
  report `Unavailable`. NaN poisons a tracker until fresh samples arrive.
- **Percentiles**: cumulative buckets are snapshotted per scrape; estimates
  interpolate within the bucket where the target quantile falls, using the
  delta between the newest snapshot and the oldest one within the window.
  Bucket-layout changes or resets clear the ring. `+Inf`-bucket hits
  saturate at the highest finite bound. "No observations in the window" is
  reported as zero observations — distinct from "cannot estimate".
- **Aggregation honesty**: fleet KV is capacity-weighted only when every
  series exposes `cache_config_info.kv_cache_size_tokens`; otherwise the
  UI shows an unweighted mean explicitly labelled as such. TTFT across a
  fleet row is shown as *worst* p95, labelled. Histogram buckets are never
  merged across endpoints (bucket compatibility across servers is not
  assumed anywhere in v1).
- **Staleness**: no successful scrape for `max(3 × interval, 5 s)` ⇒
  `STALE`; the last good snapshot stays on screen, and fleet totals exclude
  stale endpoints rather than silently counting frozen numbers.
- **Pause**: freezes automatic redraws only; collection, reduction, history,
  and recording continue. Documented in the README.

## Secret handling

- TOML stores env-var **names** (`bearer_token_env`, `header_env`); values
  are read once at startup.
- `ResolvedAuth` implements neither `Debug` nor `Clone`; resolved values
  flow only into `reqwest` default headers, which are marked
  `set_sensitive(true)`.
- Displayed URLs pass through `redact_url` (no userinfo, no query).
- HTTP errors are reduced to the innermost source message (e.g.
  "Connection refused"); reqwest's URL-bearing display form is never used.
- The redirect policy is `none`, so auth headers cannot follow a redirect
  to another origin.
- The recorder schema has no column that could hold a secret.

## Recording schema

```sql
CREATE TABLE meta    (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- meta rows: ('schema_version', '1')

CREATE TABLE samples (
    ts_ms    INTEGER NOT NULL,   -- wall-clock ms since epoch
    endpoint TEXT    NOT NULL,   -- configured endpoint name
    model    TEXT    NOT NULL,   -- model_name label
    engine   TEXT,               -- engine label (NULL when absent)
    metric   TEXT    NOT NULL,   -- state::series_id (running, waiting, …)
    value    REAL    NOT NULL
);
CREATE INDEX idx_samples_ts           ON samples (ts_ms);
CREATE INDEX idx_samples_ep_metric_ts ON samples (endpoint, metric, ts_ms);
```

Migration strategy: `schema_version` is stamped at creation. A database with
a **newer** version than the binary understands is refused (no destructive
surprises); older versions will be migrated in place by future releases
(none exist yet). Rows are the same curated series ids the fleet
charts, written in one transaction per scrape batch, WAL mode, `synchronous
= NORMAL`. Retention deletes in chunks of 10 000 rows at most every 10
minutes.

## Notes for reviewers

Areas worth extra scrutiny:

1. `metrics/histogram.rs` — quantile interpolation edge cases (empty
   windows, single-bucket layouts, saturation).
2. `collector/mod.rs::sleep_until_next` — backoff/jitter/force-refresh
   interaction; confirm no busy-loop path when channels close.
3. `state/mod.rs::ingest_metrics` — series eviction vs stale-data
   preservation; counter-bank keys include the finish reason strings.
4. Fleet totals excluding stale endpoints (`ui/fleet.rs`) — intended, but
   opinionated.
5. The recorder's drop-on-full policy and its `rows_written` verification
   counter.
