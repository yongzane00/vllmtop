# FAQ

Conceptual questions that shaped vllmtop's design, with the honest answers
the tool is built around.

## Where does the KV cache live, and what happens to it after a conversation?

The KV cache is accelerator (GPU) memory on the vLLM server holding
per-token attention key/value tensors for *in-flight* requests. It is not
on the client, and it is not a conversation store: when a request finishes,
its blocks are freed. The only cross-request reuse is the prefix cache
(matching token prefixes reuse blocks), visible as hit rates — the content
itself is never exposed. If a "conversation" spans multiple requests, each
request re-sends the history and re-fills cache (modulo prefix hits).
See [VLLM-SEMANTICS.md](VLLM-SEMANTICS.md#kv-cache).

## Can vllmtop show who is using my vLLM server?

No — and not because vllmtop chooses not to. vLLM's metrics carry no user,
session, or API-key labels, and its HTTP API exposes no request registry.
The optional requests pane (log tailing) shows request *ids* and prompt
previews from your own server's logs, which still identifies requests, not
people. Per-user accounting requires a gateway/proxy in front of vLLM that
authenticates callers — a different tool by design.

## Can I capture conversations for fine-tuning or a knowledge base?

People do build such pipelines, but at the serving layer (a logging proxy,
or vLLM's own request logging at DEBUG), not at the monitoring layer.
vllmtop deliberately never persists prompts or outputs — previews in the
requests pane are bounded, in-memory only, and excluded from the recorder
by construction. If you build capture into your serving stack, treat it as
what it is: collection of potentially personal data, with the consent and
retention obligations that implies.

## Why does the UI say `tokens/s` instead of `t/s`?

`t/s` reads as a physics unit and is ambiguous to engineers. Similarly,
request-count chart axes use integer bounds (a y-axis label of "1.1
requests" is nonsense), and rates that cannot be computed yet show `--`
rather than a fake 0.

## What's the difference between `--` and `0`?

`--` means *unavailable*: the endpoint didn't expose the metric, the first
sample hasn't landed, a counter just reset, or a day has no recorded
observations. `0` always means a measured zero. The distinction is carried
end-to-end (`Option` in every layer) because conflating them turns "I don't
know" into "nothing happened" — the worst kind of monitoring lie.

## What exactly do `served` and the token odometer count?

`served` is the sum of vLLM's finished-request counters across all finish
reasons (including errors and aborts); `tokens X in / Y out` are the
cumulative prompt/generation token counters. All are "since the server
started" — they reset honestly on a restart — and they measure *scheduler
work*, not delivered responses: tokens generated for a request that later
aborted are included. Delivered-only accounting is not derivable from
vLLM's metrics.

## Why do the daily-usage charts need recording? Why can bars show `--`?

30 days doesn't fit in memory (the in-memory history rings cap at ~68
minutes at 1 s refresh). The bars are computed from the SQLite recorder,
which is on by default; a `--` bar means "vllmtop wasn't watching that day"
— absent is not zero. Details and error bounds:
[USAGE-ACCOUNTING.md](USAGE-ACCOUNTING.md).

## Why doesn't the requests pane show prompts?

Your vLLM server is logging at INFO. Prompt previews only exist in vLLM's
logs at `VLLM_LOGGING_LEVEL=DEBUG` (with `--enable-log-requests`); at INFO
the pane still shows request id, age, and max_tokens. See
[LOG-TAILING.md](LOG-TAILING.md).

## Why is there no "images per day" chart?

vLLM exports no image-count metric. The closest thing
(`vllm:mm_cache_queries_total`) counts multimodal *cache lookups* — zero
when the cache is off, inflated by repeats — and charting it as "images"
would be dishonest. See
[VLLM-SEMANTICS.md](VLLM-SEMANTICS.md#images--multimodal).

## Why do peaks (`pk`) sometimes disappear?

Session peaks are discarded when the monitored server restarts — a peak
from before a restart would be a stale brag about a process that no longer
exists.
