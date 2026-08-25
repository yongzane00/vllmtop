# vLLM metrics: what they mean and where they end

Everything below was established against a real (sanitized) vLLM 0.24.0
`/metrics` capture (`tests/fixtures/vllm_0_24_single_engine.txt`) and by
reading the vLLM metrics source (`vllm/v1/metrics/loggers.py`,
`entrypoints/serve/utils/request_logger.py`). It is the domain knowledge the
rest of vllmtop is built on. For the exact curated-metric table see
[METRICS.md](METRICS.md).

## Types and traps

- **The metric type comes from `# TYPE`, never from the name.** vLLM exposes
  `vllm:iteration_tokens_total` as a **histogram** despite the `_total`
  suffix. Any code that pattern-matches suffixes will corrupt data.
- **Counters reset to zero when the server restarts.** A rate computed
  across a restart is negative garbage; a "since start" total silently
  shrinks. Everything downstream must be reset-aware (vllmtop flags the
  restart, suppresses the affected rates for one interval, and discards
  session peaks).
- **Partial resets happen within a family.** `vllm:request_success_total`
  has one series per `finished_reason`; after a restart they do not all
  reappear in the same scrape. Summing a half-reset family produces a
  plausible-looking wrong number, so vllmtop suppresses the whole family's
  rate for that interval.
- **Histograms are cumulative bucket counters** (`_bucket{le=...}` +
  `_sum`/`_count`, with a `+Inf` bucket). Percentiles do not exist in the
  data; they are *estimates* from bucket deltas over a time window, linearly
  interpolated inside buckets, saturating at the highest finite bound when
  the target lands in `+Inf`.
- **Names drift across versions.** Known renames handled as aliases:
  `vllm:gpu_cache_usage_perc` → `vllm:kv_cache_usage_perc`,
  `vllm:time_per_output_token_seconds` → `vllm:inter_token_latency_seconds`,
  `vllm:gpu_prefix_cache_{queries,hits}` → `vllm:prefix_cache_{queries,hits}`.
  Renames must be added as aliases, never replacements — both spellings stay
  recognized.
- **Label sets are tiny by design**: essentially `model_name` and `engine`
  (plus enum labels like `finished_reason`, waiting `reason`, prompt-token
  `source`). There is no request id, user, session, or API key label
  anywhere in `/metrics`.

## KV cache

- The KV cache holds the per-token attention key/value tensors in
  accelerator memory, allocated in fixed-size blocks
  (`--block-size` tokens per block). While a request generates, its KV
  grows token by token — this is why the endpoint view's KV bar visibly
  fills during a long generation.
- `vllm:kv_cache_usage_perc` is the *aggregate* fraction of blocks in use.
  With one active conversation it is effectively that conversation's
  footprint.
- **Capacity is not exported directly as a gauge.** It comes from the info
  metric `vllm:cache_config_info`, either as a `kv_cache_size_tokens` label
  (newer) or as `num_gpu_blocks × block_size` (older). Critically, that
  metric carries an `engine` label but **no `model_name`** — so joining
  capacity onto model series is only safe when exactly one series claims an
  engine. Multiple models sharing an engine share its cache; attributing
  the full capacity to each would double-count in capacity-weighted
  aggregation, so those series keep capacity unknown.
- **After a request/conversation ends, its KV blocks are freed** (or
  preempted earlier under pressure — see `vllm:num_preemptions_total`).
  Nothing about a conversation persists in the server's metrics. The only
  cross-request reuse is the **prefix cache**: blocks whose token prefix
  matches a new request can be reused, observable via
  `vllm:prefix_cache_queries_total` / `hits_total` (lifetime counters; a
  windowed hit rate needs deltas of both).

## Requests

- The **only finished-request counter** is `vllm:request_success_total`,
  labeled by `finished_reason` ∈ {`stop`, `length`, `abort`, `error`,
  `repetition`}. Despite the name it includes errors and aborts — "total
  requests served" = the sum over all reasons. There is no
  started-requests counter.
- In-flight state is gauges: `vllm:num_requests_running`,
  `vllm:num_requests_waiting` (+ `_by_reason`, `capacity|deferred`).
- Request *parameters* are exported only as **distributions**:
  `vllm:request_params_max_tokens`, `vllm:request_params_n`,
  `vllm:request_max_num_generation_tokens` are histograms. You can know the
  p95 of `max_tokens` across recent requests; you cannot know any single
  request's value from `/metrics`.
- `vllm:prompt_tokens_total` / `vllm:generation_tokens_total` count
  **scheduler work**: tokens processed/generated since server start,
  including work for requests that later aborted. They are "work the model
  did", not "tokens delivered to clients" — delivered-only accounting is
  not derivable from vLLM's metrics.

## Per-request and per-user visibility

- **No HTTP endpoint exposes request contents.** The OpenAI-compatible
  server's read-only routes are health/metadata only (`/metrics`, `/health`,
  `/version`, `/v1/models`, `/load` — an aggregate in-flight count,
  `/server_info` — static config). Nothing enumerates requests, prompts, or
  ids. Verified against the vLLM entrypoint source, not just the docs.
- Per-request information exists **only in the server's stdout log**, gated
  on `--enable-log-requests`: request id + `SamplingParams` at INFO, prompt
  previews only at `VLLM_LOGGING_LEVEL=DEBUG`. This is what the opt-in
  requests pane tails — see [LOG-TAILING.md](LOG-TAILING.md).
- **Per-user attribution does not exist at this layer at all.** Metrics
  have no user labels and request logs identify requests, not people. Any
  who-is-calling accounting has to live in a gateway/proxy in front of
  vLLM, outside vllmtop's read-only scope.

## Images / multimodal

- **vLLM exports no image-count metric.** The only multimodal metrics are
  `vllm:mm_cache_queries_total` and `vllm:mm_cache_hits_total` — lookups in
  the multimodal *processor cache*, in units of cached items. They stay at
  0 when MM caching is disabled and are inflated by repeated identical
  inputs, so they are a weak proxy, not a count of images. This is why the
  fleet's daily-usage section has three charts, not four: an "images/day"
  chart would either be dishonest (the proxy) or permanently `--`.

## Engines and data parallelism

- Data-parallel serving exposes the same metric names with distinct
  `engine` labels. Series must be kept separate per (model, engine):
  merging histogram buckets across engines/endpoints is only valid when
  bucket boundaries match, and percentages must never be averaged without
  capacity weighting (a 10%-full 80 GB cache and a 90%-full 8 GB cache do
  not average to 50%).
