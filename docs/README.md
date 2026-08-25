# vllmtop documentation

Start with the top-level [README](../README.md) for installation, usage,
keys, and views. The documents here go deeper, one topic per file.

## Specification & architecture

| Document | What it covers |
| --- | --- |
| [PLAN.md](PLAN.md) | Product constraints, module map, key decisions (numbered, with owner-decision history), milestones. The closest thing to a spec. |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Data flow, module tour, correctness decisions, secret handling, recording schema, the log-tailing privacy firewall. |
| [METRICS.md](METRICS.md) | The curated metric table: names, aliases across vLLM versions, and behavior when a metric is missing. |

## Domain knowledge

| Document | What it covers |
| --- | --- |
| [VLLM-SEMANTICS.md](VLLM-SEMANTICS.md) | What vLLM's metrics actually mean and where their limits are: counter/histogram traps, KV-cache lifecycle, request visibility, why images can't be counted. |
| [USAGE-ACCOUNTING.md](USAGE-ACCOUNTING.md) | How the fleet's past-30-days charts are computed: cumulative snapshots, restart-aware positive deltas, local-day bucketing, error bounds. |
| [LOG-TAILING.md](LOG-TAILING.md) | The requests pane: vLLM's log formats, the parser, tailing semantics, bounds, and privacy guarantees. |

## Contributing & operations

| Document | What it covers |
| --- | --- |
| [DEV-ENVIRONMENT.md](DEV-ENVIRONMENT.md) | Building on Windows + WSL2, the verification suite, the screenshot pipeline, live-validation protocol. |
| [LESSONS.md](LESSONS.md) | Bugs and design traps found during development, and the rules that prevent their return. |
| [FAQ.md](FAQ.md) | Conceptual questions that came up while building: KV-cache lifetime, user attribution, `--` vs `0`, what the odometers count. |

Also see [CONTRIBUTING.md](../CONTRIBUTING.md) for the development setup
and conventions.
