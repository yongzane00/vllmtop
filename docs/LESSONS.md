# Engineering lessons

Bugs and design traps actually hit while building vllmtop, and the rules
that keep them from coming back. Ordered by layer, not chronology.

## Collection

- **Interval changes caused a poll storm.** Collectors watching a
  `tokio::watch` interval channel all woke instantly on every change,
  bypassing per-endpoint backoff. Fix: `send_if_modified` on the sender and
  a re-sleep loop on the receiver (`borrow_and_update`, recompute the
  deadline, sleep again) — only a force-refresh returns immediately.
- **Every response body needs a cap.** `/metrics` was capped early
  (8 MiB), but `/version` and `/v1/models` were read uncapped — a
  misbehaving server could OOM the monitor. Rule: any network read goes
  through a capped reader (256 KiB for metadata endpoints).
- **Diagnostics need bounds too.** The Prometheus parser recorded one
  `ParseIssue` per malformed line, unbounded — 8 MiB of garbage where every
  ~4-byte line is invalid means ~2M allocations per scrape, and issue
  messages embed input tokens of arbitrary length. Fix: count all issues
  honestly (`issue_count`), store only the first 64, truncate messages to
  120 bytes on char boundaries. "Everything bounded" includes error paths.

## Metrics semantics

- **Never trust name suffixes** — `vllm:iteration_tokens_total` is a
  histogram. Type comes from `# TYPE` only.
- **Partial counter-family resets** (per-`finished_reason` series
  reappearing at different times after a restart) make a summed rate
  plausible and wrong; suppress the whole family's rate for the interval.
- **KV capacity join**: `cache_config_info` has an `engine` label but no
  `model_name`. A "single entry fallback" once let engine 1 claim
  engine 0's capacity; and attributing a shared engine's capacity to every
  model on it double-counts in capacity-weighted aggregation. Rule: join
  only when exactly one series claims the capacity; otherwise capacity
  stays unknown and the UI says "unweighted mean (capacity unknown)".
- **Honest aggregation**: counts sum; percentages average only
  capacity-weighted and labelled; latencies aggregate as "worst", never
  summed or averaged across series; histogram buckets never merge across
  endpoints/engines unless boundaries match.

## State & storage

- **Sum rates, don't integrate them.** Per-day totals from `rate × Δt`
  silently undercount (dropped batches, reset gaps, downtime). Record
  cumulative counter snapshots and take restart-aware positive deltas —
  see [USAGE-ACCOUNTING.md](USAGE-ACCOUNTING.md).
- **Retention vs test fixtures**: storage tests wrote rows at
  `ts_ms = 1000` (i.e. 1970) and retention cleanup legitimately deleted
  them — the "failing" test was correct behavior. Time-based logic tests
  must use current timestamps; the lesson is memorialized in
  `ancient_rows_are_cleaned_up_even_when_freshly_written`.
- **Additive schema changes don't need version bumps**: new metric-name
  rows and `CREATE INDEX IF NOT EXISTS` are v1-compatible; the schema
  version exists to refuse *newer* databases, not to churn.
- **WAL is the read/write contract**: the writer thread owns its
  connection; readers open separate read-only connections and never block
  it. Reads still never run on the UI thread (`spawn_blocking` + event).

## UI

- **ratatui `Paragraph` clips silently.** Appending the `served` counter to
  a status line that already measured ~115 cells at 120 columns made
  the new field invisible — no error, no wrap, just gone. Measure a line's
  worst case before appending to it; if it doesn't fit, restructure (the
  line was split in three; that strip later became the card band).
- **Absent ≠ zero must survive into charts — and widgets can defeat it.**
  ratatui's `BarChart` renders nothing for zero-height bars (absent-day
  `--` markers silently vanished) and its per-bar labels fuse into an
  unreadable digit wall at 30 narrow bars. The fix was a hand-rendered
  tape: bars for observed days, a dim low mark for measured zeros, a faint
  baseline dot for unobserved days, and sparse relative date ticks
  (`-5d … today`). Verify chart honesty against a *sparse* dataset (day
  one of recording), not just a dense demo.
- **A widget's own legend can cover the data.** ratatui's `Chart` legend is
  a bordered box drawn *inside* the plot area; at a 7-row chart it hides most
  of the line it labels. Rendering the legend into the block's title costs
  zero plot rows and doubles as a per-series live value.
- **Merging series keys into one line draws a sawtooth.** The endpoint charts
  collected every `(model, engine)` ring for a metric into a single sorted
  point list, interleaving two models' samples into a zigzag that looked like
  data. One dataset per (metric, series key), ordered deterministically so
  colours do not shuffle between frames.
- **Colour is not a channel in monochrome.** Overlaid lines were distinguished
  only by `series_color`, which returns the default style in mono — so two
  lines became one. Vary the *marker* per line as well.
- **`--` must survive the widget too.** A card whose value is unknown gets no
  unit, no bar and no sparkline: a bar drawn for an unknown fraction reads as
  a measurement. Enforced in one place (the card constructors) rather than at
  every call site.
- **Dynamic sizing beats clever clipping**: bar width derives from the
  area every frame; when 30 days can't fit, show the last N and label it
  `(last Nd)` in the title rather than cropping silently.

## Testing the UI

- **Nothing rendered a frame until late.** All ~2,200 lines of `ui/` were
  unreachable from the test suite, so a layout panic or constraint overflow
  could only be found by running the binary. `TestBackend` ships inside the
  already-compiled `ratatui-core`, so the whole render matrix (sizes × views
  × panel modes × themes) cost no new dependency.
- **Test the boundaries, not the round numbers.** Off-by-ones live at the
  exact width/height where a ladder switches rungs (36/100/150 columns, the
  minimum chart and band heights), so the sweep clusters sizes around each
  threshold instead of stepping uniformly.
- **A latent panic only a render test finds.** `((fit - 1) * step + bar_w)`
  in the usage chart underflows when no days are recorded — unreachable in
  production because the query always returns 30 days, but one empty-list
  path away from taking down the whole TUI.
- **Wall-clock assertions are the classic flaky test.** The phase-stagger
  integration test asserted start instants within ±200 ms of a 250 ms
  spacing; under full-suite load tokio wake-ups drift further and it failed
  intermittently. It now asserts the *shape* — ordering plus a generous
  envelope — while the exact arithmetic stays covered by a deterministic
  unit test.

## Rendering to SVG (screenshot pipeline)

- **SVG collapses run whitespace** in `<text>`, and a whole-line
  `textLength` stretches glyphs visibly. Render one absolutely-positioned
  `<text>` per **word**; never draw spaces (backgrounds are rect runs).
- **A fresh detached tmux pane can report a 0×0 pty** to its child; run
  `stty rows R cols C` in the pane before launching the TUI or captures
  come out blank.

## Log tailing

- **Size-based truncation detection has one blind spot**: a rotated-in
  file *longer* than the previous offset. A test for truncation initially
  "failed" because its replacement content was longer than the original —
  the code was right, the test had recreated the documented limitation.
- **Start at EOF, not backwards-read**: backfilled entries would fabricate
  the age column; `tail -f` semantics keep every displayed age honest.
- **Parse Python reprs, don't split on quotes**: vLLM logs prompts as
  Python string reprs with escapes, and truncates long ones (no closing
  quote). An escape-aware prefix decoder that tolerates a missing
  terminator handles both.

## Process

- **Live validation catches what mocks cannot.** A run against the dev box
  showed every metric as `--`: the server had been replaced by **SGLang**,
  which serves `/metrics` and `/v1/models` but names every metric
  `sglang:*`. The degradation was correct (no crash, no fake zeros, model
  and context window still read from `/v1/models`) — except the STATUS card,
  which said `IDLE` when it had never seen a running-request gauge at all.
  Unknown is not idle; that card now stays `--`. Mock servers only ever
  speak the dialect you taught them.
- **Verify before claiming.** The dev endpoint is sometimes unreachable;
  a run that never scraped proves nothing about live behavior (it does
  prove failure modes — report it as that). `curl` first.
- **Features that "already exist" happen**: two requested features
  (tokens-per-request percentiles, failure-count display) turned out to be
  already implemented. Grep before building; report honestly instead of
  re-implementing.
- **Edition 2024**: `std::env::set_var` is unsafe — env access is injected
  as closures for testability; clippy prefers `if … && let …` chains over
  nested ifs.
