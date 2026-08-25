# Log tailing and the requests pane

The endpoint view's requests pane (age / prompt preview / request id /
max_tokens) is the one deliberate exception to "metrics only". This
documents how it works, its exact limits, and the privacy rules that must
never regress. Code: `src/logtail/` (tailer + parser),
`src/state/requests.rs` (ring), `src/ui/endpoint.rs::draw_requests`.

> **History**: the original v1 spec excluded per-request visibility and log
> ingestion entirely. The owner reversed that in 2026-08 (PLAN.md key
> decision 11) after establishing that vLLM's HTTP API cannot provide this
> data at all — see below.

## Why logs

vLLM's OpenAI-compatible server has **no read-only HTTP endpoint** exposing
request contents; `/metrics` labels carry no request ids. The only source
is the server's own stdout, produced by `RequestLogger` when the server
runs with `--enable-log-requests`:

```
INFO 08-24 10:23:45 [request_logger.py:63] Received request chatcmpl-8e66f5c2: params: SamplingParams(n=1, temperature=0.7, ..., max_tokens=1024, ...), lora_request: None.
DEBUG 08-24 10:23:45 [request_logger.py:53] Request chatcmpl-8e66f5c2 details: prompt: 'An example question...', prompt_token_ids: ..., prompt_embeds shape: ...
INFO 08-24 10:23:47 [request_logger.py:98] Generated response chatcmpl-8e66f5c2: output: '...', finish_reason: stop
```

Consequences users must know:

- request id + params (including `max_tokens`) appear at **INFO**;
- **prompt previews exist only at `VLLM_LOGGING_LEVEL=DEBUG`** — otherwise
  the prompt column is honestly `--`;
- the log file must be **locally readable** by vllmtop (same host or a
  mounted path). Remote endpoints without a log show a hint, not fake data.

## The tailer (`logtail/mod.rs`)

One tokio task per endpoint with `log_file` configured. Every 500 ms:

- **Reopen the path** (not a held handle): rotation-by-rename is followed
  automatically because the read always targets whatever file currently
  has that name.
- **`len < offset` ⇒ truncation/copytruncate** ⇒ restart from offset 0.
  The one undetectable case: a rotated-in file *longer* than the previous
  offset (size-based detection can't see it; worst case one tick's lines
  are skipped). Documented, not hidden.
- **First open seeks to EOF** — `tail -f` semantics. Backfilling old
  entries would fabricate ages ("age since seen" would lie), so only
  requests arriving after vllmtop starts appear. A file that appears
  *after* startup is read from its beginning (all of it is new).
- Bounded everywhere: ≤ 256 KiB read per tick (backlogs drain over ticks),
  ≤ 16 KiB per line (oversized lines are skipped to their newline),
  ≤ 256 parsed events per batch (newest kept), partial trailing lines
  carried between ticks, non-UTF-8 handled lossily.
- Batches go over the same bounded channel as everything else
  (`AppEvent::RequestLog`); a full channel drops that tick's batch.

## The parser (`logtail/parse.rs`)

Std-only substring scanning — no regex crate, same reasoning as the
Prometheus parser (a small, table-tested parser beats a dependency for a
fixed grammar). Rules worth knowing:

- ANSI CSI/OSC escapes are stripped first (vLLM colorizes stdout).
- Request id: text between the marker and the next `:`, rejected if empty
  or containing spaces (truncated lines), capped at 64 bytes.
- `max_tokens`: first `max_tokens=` in the params repr, value up to `,` or
  `)`. Nested parens in `SamplingParams(...)` don't matter because the key
  appears exactly once; literal `None` maps to `None` (renders `--`).
- Prompt preview: a **Python string-repr prefix decoder** — honors
  `\'`/`\"`/`\\`, maps `\n`/`\t`/`\r` to spaces, scrubs control characters
  (terminal-corruption defense), stops at the unescaped closing quote *or
  end of line* (vLLM truncates long prompts via `--max-log-len`, so a
  missing closing quote keeps the prefix), and caps at 120 bytes on a char
  boundary.
- `Generated response` lines mark the entry finished (`finish_reason`
  captured); finished rows render dimmed.

## The ring (`state/requests.rs`)

Bounded `VecDeque` of 200 entries per endpoint, newest at the back.
`Details`/`Finished` events merge into the matching id by scanning from the
back (recent ids live there; 200 entries needs no index). Unknown ids —
seen before startup or already evicted — are ignored: a stub row with no
honest age would be dishonest. Duplicate `Received` refreshes in place.

## Privacy firewall (must never regress)

- Prompt previews exist **only** in this in-memory ring: never written to
  the SQLite recorder, never `tracing`-logged, never persisted anywhere.
- The mechanism, not a convention: the recorder's only inputs are
  `EndpointState::current_samples()` and `cumulative_samples()`, neither of
  which can see the request ring. Any future recorder feed must preserve
  that separation.
- vllmtop still never proxies or inspects inference traffic; the tailer is
  a read-only consumer of a log the operator already produces, opt-in per
  endpoint, local files only.
- Per-**user** attribution remains impossible and out of scope: request
  logs identify requests, not people.

## Configuration

```toml
[[endpoints]]
name = "local"
url  = "http://127.0.0.1:8000"
log_file = "/var/log/vllm/server.log"
```

TOML-only for v1 — a `-e` CLI suffix was rejected because paths collide
with the `NAME=URL[@CAP]` grammar (`=`, `@`, `:` all legal in paths). The
pane's empty states walk the user through setup: no `log_file` configured →
pointer to the example config; file missing → the path it looked for;
tailing but quiet → the `--enable-log-requests` / DEBUG requirements.
