# Metric compatibility

vllmtop detects capabilities from what an endpoint actually exposes — there
are no version checks. This table lists the curated metrics, their aliases,
and what happens when they are missing. Everything else the server exposes is
parsed and tolerated; curation simply ignores it.

Ground truth: a live vLLM 0.24.0 capture (sanitized as
`tests/fixtures/vllm_0_24_single_engine.txt`) plus the vLLM metrics
design/usage docs and source as of August 2026.

## Labels and series identity

Since vLLM 0.8.5 every `vllm:*` series carries `model_name` **and** `engine`
(the engine index as a string, `"0"` even for single-engine deployments;
data-parallel servers export one series per engine). vllmtop keys every
series by `(model_name, engine)` and never collapses the dimensions.
Pre-0.8.5 servers without the `engine` label work too (the key's engine part
is simply absent).

`finished_reason` on `vllm:request_success_total` currently takes values
`stop`, `length`, `abort`, `error`, `repetition`; vllmtop accepts arbitrary
values and treats `error` + `abort` as the error/abort signal.

## Curated metrics

| Quantity | Current name | Accepted aliases (older vLLM) | When missing |
| --- | --- | --- | --- |
| Running requests | `vllm:num_requests_running` | — | `--` |
| Waiting requests | `vllm:num_requests_waiting` | — | `--` |
| Waiting by reason | `vllm:num_requests_waiting_by_reason` (label `reason`: `capacity`, `deferred`) | — | reason breakdown hidden |
| KV-cache usage (0–1) | `vllm:kv_cache_usage_perc` | `vllm:gpu_cache_usage_perc` (pre-V1; removed ~0.11) | `--`, no bar |
| KV capacity (tokens) | `vllm:cache_config_info` label `kv_cache_size_tokens` | computed `num_gpu_blocks × block_size` when only those labels exist | fleet KV falls back to labelled unweighted mean |
| Prompt tokens | `vllm:prompt_tokens_total` | — | prompt t/s shows `--` |
| Generation tokens | `vllm:generation_tokens_total` | — | gen t/s shows `--` |
| Completions | `vllm:request_success_total` (label `finished_reason`) | — | completion rate + finish reasons show `--` |
| Preemptions | `vllm:num_preemptions_total` | — | `--` |
| Prefix-cache queries | `vllm:prefix_cache_queries_total` | `vllm:gpu_prefix_cache_queries_total` (renamed v0.9.2) | hit rate `--` |
| Prefix-cache hits | `vllm:prefix_cache_hits_total` | `vllm:gpu_prefix_cache_hits_total` | hit rate `--` |
| External prefix cache | `vllm:external_prefix_cache_queries_total` / `..._hits_total` | — | row hidden |
| TTFT | `vllm:time_to_first_token_seconds` (histogram) | — | `--` |
| Inter-token latency | `vllm:inter_token_latency_seconds` (histogram) | `vllm:time_per_output_token_seconds` (deprecated 0.11, removed later) | `--` |
| E2E latency | `vllm:e2e_request_latency_seconds` | — | `--` |
| Queue time | `vllm:request_queue_time_seconds` | — | `--` |
| Prefill time | `vllm:request_prefill_time_seconds` | — | `--` |
| Decode time | `vllm:request_decode_time_seconds` | — | `--` |
| Inference time | `vllm:request_inference_time_seconds` | — | `--` |
| Prompt tokens/request | `vllm:request_prompt_tokens` (histogram) | — | `--` |
| Generation tokens/request | `vllm:request_generation_tokens` (histogram) | — | `--` |

Adding an alias is a one-line change in `src/metrics/normalize.rs` — that
file is the only place metric name strings exist.

## Known traps handled

- **`vllm:iteration_tokens_total` is a histogram** despite the `_total`
  suffix. The parser derives types exclusively from `# TYPE` lines.
- `_created` companion series (from `prometheus_client`) are separate gauge
  families; curation ignores them.
- `vllm:kv_cache_usage_perc` is a **0–1 fraction**, not 0–100.
- Summaries without quantiles (`http_request_size_bytes`) parse fine.
- `python_*`, `process_*`, `http_*` families are parsed but not displayed —
  vllmtop deliberately does not become a host monitor.
- Backend-specific families (e.g. spec-decode counters, NIXL/Mooncake
  KV-connector metrics, MFU counters) and future metrics are tolerated
  without breaking anything.

## Not curated on purpose (V0-era, removed upstream)

`vllm:num_requests_swapped`, `vllm:cpu_cache_usage_perc`,
`vllm:gpu_prefix_cache_hit_rate` / `cpu_...` (replaced by queries+hits
counters), `vllm:time_in_queue_requests`,
`vllm:avg_prompt_throughput_toks_per_s`,
`vllm:avg_generation_throughput_toks_per_s`,
`vllm:request_params_best_of`, `vllm:model_forward_time_milliseconds`,
`vllm:model_execute_time_milliseconds`. If an old server exposes them they
are ignored; the fleet/detail views rely on the still-present counters
instead (throughputs are computed from token counters, queue time from its
histogram).

## vLLM deprecation policy (upstream)

Metrics deprecated in version X.Y are hidden in X.Y+1 (unhidden by
`--show-hidden-metrics-for-version=X.Y`) and removed in X.Y+2. vllmtop's
alias table covers the renames that have gone through this pipeline so far;
when a new rename lands upstream, add the old name as an alias rather than
switching, so both eras keep working.
