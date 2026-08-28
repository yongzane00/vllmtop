#!/usr/bin/env bash
# Regenerate docs/img/{fleet,endpoint}.svg from mock vLLM servers, a seeded
# 30-day usage database, and a synthetic request log.
#
# Run inside Linux/WSL from the repo root:  bash scripts/screenshot/screenshot.sh
# Requires: cargo, tmux, python3. Never touches a real vLLM server.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$REPO/scripts/screenshot"
export PATH="$HOME/.cargo/bin:$PATH" # non-login shells (wsl -e) miss it
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/build/vllmtop-target}"
WORK="$HOME/vllmtop-shots"
SESSION=vtshot
COLS=120
ROWS=32
WARMUP="${WARMUP:-45}" # seconds of history before capturing

mkdir -p "$WORK"
cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  [ -n "${PID_A:-}" ] && kill "$PID_A" 2>/dev/null || true
  [ -n "${PID_B:-}" ] && kill "$PID_B" 2>/dev/null || true
  [ -n "${FEEDER:-}" ] && kill "$FEEDER" 2>/dev/null || true
}
trap cleanup EXIT

echo "== build release =="
(cd "$REPO" && cargo build --release)
BIN="$CARGO_TARGET_DIR/release/vllmtop"

echo "== seed 30-day usage database =="
rm -f "$WORK/usage.db" "$WORK/usage.db-wal" "$WORK/usage.db-shm"
python3 "$HERE/seed_usage.py" "$WORK/usage.db"

echo "== start mock servers on 18001/18002 =="
python3 "$HERE/mock_vllm.py" --port 18001 --profile a &
PID_A=$!
python3 "$HERE/mock_vllm.py" --port 18002 --profile b &
PID_B=$!
sleep 1
curl -fsS http://127.0.0.1:18001/metrics >/dev/null
curl -fsS http://127.0.0.1:18002/metrics >/dev/null

echo "== start synthetic request-log feeder =="
LOG="$WORK/vllm.log"
: >"$LOG"
(
  prompts=(
    "Explain how the KV cache grows during decoding, in one paragraph."
    "<|im_start|>system # Tools You have access to the example toolset"
    "Summarize the example-org quarterly infrastructure report."
    "Translate 'throughput is a property of the whole system' into French."
    "Write a haiku about batched inference."
  )
  i=0
  while true; do
    id=$(printf 'chatcmpl-%04x%04x' "$RANDOM" "$RANDOM")
    mt=$(((RANDOM % 900) + 100))
    p=${prompts[$((i % ${#prompts[@]}))]}
    {
      echo "INFO 08-25 10:00:00 [request_logger.py:63] Received request $id: params: SamplingParams(n=1, temperature=0.7, top_p=1.0, stop=[], max_tokens=$mt, guided_decoding=None), lora_request: None."
      echo "DEBUG 08-25 10:00:00 [request_logger.py:53] Request $id details: prompt: '$p', prompt_token_ids: None, prompt_embeds shape: None."
    } >>"$LOG"
    sleep 1.4
    echo "INFO 08-25 10:00:01 [request_logger.py:98] Generated response $id: output: 'An example answer.', finish_reason: stop" >>"$LOG"
    i=$((i + 1))
  done
) &
FEEDER=$!

echo "== write shoot config =="
cat >"$WORK/shoot.toml" <<EOF
refresh_interval_ms = 1000
record_path = "$WORK/usage.db"

[[endpoints]]
name = "local"
url = "http://127.0.0.1:18001"
max_running = 8
log_file = "$LOG"

[[endpoints]]
name = "spark-a"
url = "http://127.0.0.1:18002"
EOF

echo "== run vllmtop in a ${COLS}x${ROWS} tmux pane =="
tmux kill-session -t "$SESSION" 2>/dev/null || true
tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS"
# A fresh detached pane can report 0x0 to the child; pin the size first.
tmux send-keys -t "$SESSION" \
  "stty rows $ROWS cols $COLS; $BIN --config $WORK/shoot.toml" Enter

echo "== warm up ${WARMUP}s of history =="
sleep "$WARMUP"

echo "== capture fleet view =="
tmux send-keys -t "$SESSION" 1
sleep 2
tmux capture-pane -t "$SESSION" -e -p >"$WORK/fleet.ans"

echo "== capture endpoint view (charts + requests) =="
tmux send-keys -t "$SESSION" 2
sleep 2
tmux capture-pane -t "$SESSION" -e -p >"$WORK/endpoint.ans"

tmux send-keys -t "$SESSION" q
sleep 1

echo "== render SVGs =="
python3 "$HERE/ans2svg.py" "$WORK/fleet.ans" "$REPO/docs/img/fleet.svg"
python3 "$HERE/ans2svg.py" "$WORK/endpoint.ans" "$REPO/docs/img/endpoint.svg"
echo "wrote docs/img/fleet.svg and docs/img/endpoint.svg"
