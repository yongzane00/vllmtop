# Screenshot pipeline

Regenerates the README screenshots (`docs/img/fleet.svg`,
`docs/img/endpoint.svg`) from **mock** vLLM servers — no GPU, no real
endpoint, no network beyond localhost.

```bash
# inside Linux/WSL, from the repo root
bash scripts/screenshot/screenshot.sh

# longer/shorter history in the charts:
WARMUP=90 bash scripts/screenshot/screenshot.sh
```

Requires `cargo`, `tmux`, `python3` (stdlib only).

## How it works

1. `mock_vllm.py` serves `/metrics` (vLLM 0.24-era metric names with
   smoothly evolving, monotonic values), `/health`, `/version`,
   `/v1/models` on ports 18001/18002 — two different "servers" with
   different models and load shapes. Model names are sanitized
   (`example-org/...`).
2. `screenshot.sh` builds the release binary, runs it in a detached
   120×32 tmux pane (with an explicit `stty rows/cols` — a fresh pane can
   otherwise report 0×0 to the child), lets ~45 s of history accumulate,
   and captures each view with `tmux capture-pane -e`.
3. `ans2svg.py` renders the ANSI capture to SVG: backgrounds as rect runs,
   text as one absolutely-positioned `<text>` per **word** (whole-line
   `textLength` visibly distorts glyph spacing; per-word it does not).

The captures land in `~/vllmtop-shots/*.ans` if you want to re-render
without re-running the TUI.
