#!/usr/bin/env python3
"""A stdlib-only mock vLLM server for screenshots and local UI testing.

Serves /metrics (Prometheus text, vLLM 0.24-era names), /health, /version,
and /v1/models. Values evolve smoothly with wall time so vllmtop's charts,
rates, and percentile estimates all show believable, moving data.

Everything is deterministic given wall time; counters are monotonic.
Only sanitized example model names are used.
"""

import argparse
import json
import math
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

START = time.monotonic()
# Wall-clock start, for process_start_time_seconds (uptime).
START_WALL = time.time() - 24_460  # pretend the server has been up ~6.8 h


def build_metrics(cfg):
    t = time.monotonic() - START
    m = cfg["model"]
    lab = f'engine="0",model_name="{m}"'
    out = []

    def gauge(name, value, labels=lab):
        out.append(f"# TYPE {name} gauge")
        out.append(f"{name}{{{labels}}} {value}")

    def counter(name, value, labels=lab):
        out.append(f"# TYPE {name} counter")
        out.append(f"{name}{{{labels}}} {value}")

    # Monotonic counter: base rate plus a bounded wave (derivative >= 0).
    def mono(rate, period, phase=0.0):
        amp = rate * 0.6
        return rate * t + amp * period * (1 - math.cos(t / period + phase))

    running = max(
        0.0,
        round(
            cfg["run_base"]
            + cfg["run_amp"] * math.sin(t / 13 + cfg["phase"])
            + 0.9 * math.sin(t / 5 + cfg["phase"] * 2)
        ),
    )
    waiting = max(0.0, round(1.6 * math.sin(t / 29 + cfg["phase"]) - 0.9))
    kv = min(0.97, max(0.02, cfg["kv_base"] + cfg["kv_amp"] * math.sin(t / 23 + cfg["phase"])))

    gauge("vllm:num_requests_running", running)
    gauge("vllm:num_requests_waiting", waiting)
    gauge("vllm:kv_cache_usage_perc", round(kv, 4))
    out.append("# TYPE vllm:cache_config_info gauge")
    # kv_cache_memory_bytes is the literal string "None" on real servers.
    out.append(
        'vllm:cache_config_info{'
        f'engine="0",num_gpu_blocks="{cfg["blocks"]}",block_size="16",'
        f'kv_cache_size_tokens="{cfg["blocks"] * 16}",cache_dtype="auto",'
        'gpu_memory_utilization="0.90",kv_cache_memory_bytes="None",'
        'enable_prefix_caching="True"} 1'
    )

    # Endpoint-global metrics: unlabeled process series from the default
    # prometheus_client collector, plus the FastAPI instrumentator's counts.
    out.append("# TYPE process_start_time_seconds gauge")
    out.append(f"process_start_time_seconds {START_WALL:.2f}")
    out.append("# TYPE process_resident_memory_bytes gauge")
    out.append(f"process_resident_memory_bytes {cfg['rss_bytes']:.1f}")
    out.append("# TYPE process_cpu_seconds_total counter")
    out.append(f"process_cpu_seconds_total {t * cfg['cpu_frac']:.2f}")
    out.append("# TYPE process_open_fds gauge")
    out.append(f"process_open_fds {cfg['fds']}")
    out.append("# TYPE process_max_fds gauge")
    out.append("process_max_fds 65535.0")
    out.append("# TYPE http_requests_total counter")
    ok = mono(cfg["req_rate"], 19)
    out.append(
        'http_requests_total{handler="/v1/chat/completions",method="POST",'
        f'status="2xx"}} {int(ok)}.0'
    )
    out.append(
        f'http_requests_total{{handler="/v1/models",method="GET",status="2xx"}} {int(t / 3)}.0'
    )
    out.append(
        'http_requests_total{handler="/v1/chat/completions",method="POST",'
        f'status="5xx"}} {cfg["http_5xx"]}'
    )

    counter("vllm:prompt_tokens_total", round(mono(cfg["prompt_tps"], 17), 1))
    counter("vllm:generation_tokens_total", round(mono(cfg["gen_tps"], 11), 1))
    counter("vllm:num_preemptions_total", int(t / 240))

    total_reqs = mono(cfg["req_rate"], 19)
    out.append("# TYPE vllm:request_success_total counter")
    for reason, share in (("stop", 0.92), ("length", 0.06), ("abort", 0.02)):
        out.append(
            f'vllm:request_success_total{{{lab},finished_reason="{reason}"}} '
            f"{int(total_reqs * share)}.0"
        )

    queries = mono(cfg["prompt_tps"] * 0.9, 17)
    counter("vllm:prefix_cache_queries_total", round(queries, 1))
    counter("vllm:prefix_cache_hits_total", round(queries * cfg["hit_rate"], 1))

    # Histograms: cumulative buckets that grow with total_reqs while keeping
    # a fixed per-server shape, so windowed percentile estimates are stable
    # but non-trivial.
    def histogram(name, buckets, weights, scale=1.0):
        out.append(f"# TYPE {name} histogram")
        total = total_reqs * scale
        acc = 0.0
        cum = []
        for w in weights:
            acc += w
            cum.append(acc)
        norm = acc
        for le, c in zip(buckets, cum):
            out.append(f'{name}_bucket{{{lab},le="{le}"}} {round(total * c / norm, 1)}')
        out.append(f'{name}_bucket{{{lab},le="+Inf"}} {round(total, 1)}')
        mid = sum(
            w * float(b) for w, b in zip(weights, buckets)
        ) / norm
        out.append(f"{name}_sum{{{lab}}} {round(total * mid, 2)}")
        out.append(f"{name}_count{{{lab}}} {round(total, 1)}")

    ttft_b = ["0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1.0", "2.5", "5.0"]
    lat_b = ["0.05", "0.1", "0.25", "0.5", "1.0", "2.5", "5.0", "10.0", "30.0"]
    itl_b = ["0.005", "0.01", "0.02", "0.05", "0.1", "0.25", "0.5"]
    histogram("vllm:time_to_first_token_seconds", ttft_b, cfg["ttft_w"])
    histogram("vllm:inter_token_latency_seconds", itl_b, cfg["itl_w"], scale=40)
    histogram("vllm:e2e_request_latency_seconds", lat_b, cfg["e2e_w"])
    histogram("vllm:request_queue_time_seconds", lat_b, [30, 40, 20, 6, 2, 1, 0.5, 0.3, 0.2])
    histogram("vllm:request_prefill_time_seconds", lat_b, [10, 30, 35, 15, 6, 2, 1, 0.6, 0.4])
    histogram("vllm:request_decode_time_seconds", lat_b, [1, 3, 10, 25, 30, 20, 7, 3, 1])
    tok_b = ["1", "10", "50", "100", "250", "500", "1000", "2500"]
    histogram("vllm:request_prompt_tokens", tok_b, [1, 6, 22, 30, 24, 11, 4, 2])
    histogram("vllm:request_generation_tokens", tok_b, [4, 14, 30, 26, 16, 7, 2.5, 0.5])
    return "\n".join(out) + "\n"


class Handler(BaseHTTPRequestHandler):
    cfg = None

    def do_GET(self):
        if self.path == "/metrics":
            body = build_metrics(self.cfg).encode()
            ctype = "text/plain; version=0.0.4"
        elif self.path == "/health":
            body, ctype = b"", "text/plain"
        elif self.path == "/version":
            body = json.dumps({"version": "0.24.0"}).encode()
            ctype = "application/json"
        elif self.path == "/v1/models":
            body = json.dumps(
                {
                    "object": "list",
                    "data": [
                        {
                            "id": self.cfg["model"],
                            "object": "model",
                            "root": "/models/" + self.cfg["model"].split("/")[-1],
                            "max_model_len": self.cfg["max_model_len"],
                        }
                    ],
                }
            ).encode()
            ctype = "application/json"
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


PROFILES = {
    "a": dict(
        model="example-org/example-model-27B",
        req_rate=0.8, prompt_tps=220.0, gen_tps=48.0,
        run_base=3.2, run_amp=2.6, kv_base=0.42, kv_amp=0.22,
        blocks=4131, hit_rate=0.58, phase=0.0,
        rss_bytes=2.53e9, cpu_frac=0.31, fds=63, http_5xx=1, max_model_len=262144,
        ttft_w=[5, 20, 35, 22, 10, 5, 2, 0.7, 0.3],
        itl_w=[10, 30, 35, 18, 5, 1.5, 0.5],
        e2e_w=[1, 3, 8, 18, 30, 25, 10, 4, 1],
    ),
    "b": dict(
        model="example-org/example-model-7B",
        req_rate=2.2, prompt_tps=520.0, gen_tps=130.0,
        run_base=1.8, run_amp=1.5, kv_base=0.22, kv_amp=0.13,
        blocks=8265, hit_rate=0.71, phase=2.1,
        rss_bytes=1.71e9, cpu_frac=0.44, fds=48, http_5xx=0, max_model_len=131072,
        ttft_w=[15, 35, 30, 12, 5, 2, 0.7, 0.2, 0.1],
        itl_w=[25, 40, 25, 8, 1.5, 0.4, 0.1],
        e2e_w=[4, 10, 22, 30, 20, 9, 3, 1.5, 0.5],
    ),
}

if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--profile", choices=sorted(PROFILES), default="a")
    args = ap.parse_args()
    Handler.cfg = PROFILES[args.profile]
    ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()
