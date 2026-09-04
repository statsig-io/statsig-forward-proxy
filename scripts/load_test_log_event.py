#!/usr/bin/env python3
"""Generate synthetic load for a fixed period and report peak throughput."""

from __future__ import annotations

import argparse
import json
import random
import ssl
import threading
import time
from collections import defaultdict
from dataclasses import dataclass, field
from http.client import HTTPConnection, HTTPSConnection
from pathlib import Path
from typing import Dict, List, Tuple
from urllib.parse import urlparse


def _parse_positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError(f"expected positive int, got {value}")
    return parsed


def _parse_positive_float(value: str) -> float:
    parsed = float(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError(f"expected positive float, got {value}")
    return parsed


def _parse_ratio(value: str) -> float:
    parsed = float(value)
    if parsed < 0 or parsed > 1:
        raise argparse.ArgumentTypeError(f"expected ratio in [0, 1], got {value}")
    return parsed


def _parse_header(value: str) -> Tuple[str, str]:
    if ":" not in value:
        raise argparse.ArgumentTypeError("header must be in 'Name: Value' form")
    name, raw = value.split(":", 1)
    name = name.strip()
    raw = raw.strip()
    if not name:
        raise argparse.ArgumentTypeError("header name cannot be empty")
    return name, raw


def _percentile(sorted_values: List[float], pct: float) -> float:
    if not sorted_values:
        return 0.0
    if len(sorted_values) == 1:
        return sorted_values[0]
    rank = (pct / 100.0) * (len(sorted_values) - 1)
    low = int(rank)
    high = min(low + 1, len(sorted_values) - 1)
    if low == high:
        return sorted_values[low]
    frac = rank - low
    return sorted_values[low] + (sorted_values[high] - sorted_values[low]) * frac


def _moving_average_peak(buckets: List[int], window_s: int) -> float:
    if not buckets:
        return 0.0
    if window_s <= 1:
        return float(max(buckets))
    if len(buckets) < window_s:
        return sum(buckets) / len(buckets)
    current = sum(buckets[:window_s])
    peak = current
    for idx in range(window_s, len(buckets)):
        current += buckets[idx]
        current -= buckets[idx - window_s]
        if current > peak:
            peak = current
    return peak / window_s


def _build_connection(parsed, timeout_seconds: float, insecure: bool):
    port = parsed.port
    if parsed.scheme == "https":
        if port is None:
            port = 443
        context = ssl._create_unverified_context() if insecure else None
        return HTTPSConnection(parsed.hostname, port, timeout=timeout_seconds, context=context)

    if port is None:
        port = 80
    return HTTPConnection(parsed.hostname, port, timeout=timeout_seconds)


def _make_gate_exposure(seed: int, event_time_ms: int) -> Dict:
    return {
        "eventName": "statsig::gate_exposure",
        "user": {
            "userID": f"user-{seed % 200_000}",
            "customIDs": {
                "deviceID": f"device-{seed % 500_000}",
                "sessionID": f"session-{seed % 20_000}",
            },
        },
        "time": event_time_ms,
        "metadata": {
            "gate": f"gate_{seed % 256}",
            "ruleID": f"rule_{seed % 64}",
            "gateValue": bool(seed % 2),
        },
        "statsigMetadata": {"stableID": f"stable-{seed % 50_000}"},
    }


def _make_config_exposure(seed: int, event_time_ms: int) -> Dict:
    return {
        "eventName": "statsig::config_exposure",
        "user": {"userID": f"user-{seed % 200_000}"},
        "time": event_time_ms,
        "metadata": {
            "config": f"config_{seed % 128}",
            "ruleID": f"rule_{seed % 64}",
        },
        "statsigMetadata": {"stableID": f"stable-{seed % 50_000}"},
    }


def _build_synthetic_log_event_body(
    seed: int,
    events_per_request: int,
    duplicate_ratio: float,
) -> bytes:
    unique_count = max(1, int(round(events_per_request * (1.0 - duplicate_ratio))))
    duplicate_count = max(0, events_per_request - unique_count)
    base_time = int(time.time() * 1000)

    unique_events = []
    for i in range(unique_count):
        event_seed = (seed * events_per_request) + i
        # Keep timestamps inside a small minute window so dedupe-key rounding is exercised.
        event_time_ms = base_time - (event_seed % 45) * 1_000
        if event_seed % 2 == 0:
            unique_events.append(_make_gate_exposure(event_seed, event_time_ms))
        else:
            unique_events.append(_make_config_exposure(event_seed, event_time_ms))

    events = list(unique_events)
    for i in range(duplicate_count):
        events.append(unique_events[i % len(unique_events)])

    payload = {
        "events": events,
        "statsigMetadata": {"stableID": f"global-stable-{seed % 2048}"},
    }
    return json.dumps(payload, separators=(",", ":"), ensure_ascii=True).encode("utf-8")


def _build_body_pool(
    body_file: Path | None,
    pool_size: int,
    events_per_request: int,
    duplicate_ratio: float,
) -> List[bytes]:
    if body_file is not None:
        data = body_file.read_bytes()
        return [data]

    bodies = []
    for seed in range(pool_size):
        bodies.append(
            _build_synthetic_log_event_body(
                seed=seed,
                events_per_request=events_per_request,
                duplicate_ratio=duplicate_ratio,
            )
        )
    return bodies


@dataclass
class WorkerResult:
    attempted: int = 0
    responses: int = 0
    success_2xx: int = 0
    non_2xx: int = 0
    transport_errors: int = 0
    response_bytes: int = 0
    per_second_responses: Dict[int, int] = field(default_factory=lambda: defaultdict(int))
    latency_samples_ms: List[float] = field(default_factory=list)


@dataclass
class AggregatedResult:
    attempted: int = 0
    responses: int = 0
    success_2xx: int = 0
    non_2xx: int = 0
    transport_errors: int = 0
    response_bytes: int = 0
    per_second_responses: Dict[int, int] = field(default_factory=lambda: defaultdict(int))
    latency_samples_ms: List[float] = field(default_factory=list)

    def merge(self, worker: WorkerResult) -> None:
        self.attempted += worker.attempted
        self.responses += worker.responses
        self.success_2xx += worker.success_2xx
        self.non_2xx += worker.non_2xx
        self.transport_errors += worker.transport_errors
        self.response_bytes += worker.response_bytes
        for second, count in worker.per_second_responses.items():
            self.per_second_responses[second] += count
        self.latency_samples_ms.extend(worker.latency_samples_ms)


def _worker_main(
    worker_id: int,
    parsed_url,
    method: str,
    path_query: str,
    headers: Dict[str, str],
    body_pool: List[bytes],
    timeout_seconds: float,
    insecure_https: bool,
    warmup_ends_at: float,
    test_ends_at: float,
    test_start: float,
    latency_sample_ratio: float,
    output: List[WorkerResult],
) -> None:
    local = WorkerResult()
    rng = random.Random((worker_id + 1) * 7919)
    pool_size = len(body_pool)
    body_idx = worker_id % pool_size
    conn = None

    while True:
        now = time.monotonic()
        if now >= test_ends_at:
            break

        body = None
        if method == "POST":
            body = body_pool[body_idx]
            body_idx = (body_idx + 1) % pool_size

        start = time.perf_counter()
        complete_time = time.monotonic()
        latency_ms = 0.0

        try:
            if conn is None:
                conn = _build_connection(parsed_url, timeout_seconds, insecure_https)
            conn.request(method, path_query, body=body, headers=headers)
            resp = conn.getresponse()
            body_bytes = resp.read()

            complete_time = time.monotonic()
            latency_ms = (time.perf_counter() - start) * 1000.0
            if complete_time >= warmup_ends_at and complete_time < test_ends_at:
                local.attempted += 1
                local.responses += 1
                local.response_bytes += len(body_bytes)
                second = int(complete_time - test_start)
                if second >= 0:
                    local.per_second_responses[second] += 1
                if 200 <= resp.status < 300:
                    local.success_2xx += 1
                else:
                    local.non_2xx += 1
                if latency_sample_ratio >= 1.0 or rng.random() < latency_sample_ratio:
                    local.latency_samples_ms.append(latency_ms)
        except Exception:
            complete_time = time.monotonic()
            if complete_time >= warmup_ends_at and complete_time < test_ends_at:
                local.attempted += 1
                local.transport_errors += 1
            if conn is not None:
                try:
                    conn.close()
                except Exception:
                    pass
            conn = None

    if conn is not None:
        try:
            conn.close()
        except Exception:
            pass
    output[worker_id] = local


def _print_summary(
    result: AggregatedResult,
    duration_seconds: float,
    print_top_seconds: int,
    moving_average_window_seconds: int,
) -> None:
    buckets_len = max(1, int(duration_seconds))
    per_second = [result.per_second_responses.get(i, 0) for i in range(buckets_len)]
    peak_count = max(per_second) if per_second else 0
    peak_second = per_second.index(peak_count) if per_second else 0
    overall_rps = result.responses / duration_seconds
    attempted_rps = result.attempted / duration_seconds
    avg_mbps = (result.response_bytes * 8 / 1_000_000) / duration_seconds
    peak_window_rps = _moving_average_peak(per_second, moving_average_window_seconds)

    print("")
    print("Load Test Summary")
    print("-----------------")
    print(f"Requests attempted      : {result.attempted}")
    print(f"Responses received      : {result.responses}")
    print(f"2xx responses           : {result.success_2xx}")
    print(f"Non-2xx responses       : {result.non_2xx}")
    print(f"Transport errors        : {result.transport_errors}")
    print(f"Attempt throughput      : {attempted_rps:.2f} req/s")
    print(f"Response throughput     : {overall_rps:.2f} req/s")
    print(f"Peak 1s throughput      : {peak_count:.2f} req/s at +{peak_second}s")
    print(
        f"Peak {moving_average_window_seconds}s avg throughput: "
        f"{peak_window_rps:.2f} req/s"
    )
    print(f"Average response bitrate: {avg_mbps:.2f} Mb/s")

    samples = sorted(result.latency_samples_ms)
    if samples:
        print("")
        print(f"Latency samples         : {len(samples)}")
        print(f"p50 latency             : {_percentile(samples, 50):.2f} ms")
        print(f"p95 latency             : {_percentile(samples, 95):.2f} ms")
        print(f"p99 latency             : {_percentile(samples, 99):.2f} ms")
    else:
        print("")
        print("Latency samples         : 0 (increase --latency-sample-ratio)")

    if print_top_seconds > 0:
        print("")
        print(f"Top {print_top_seconds} one-second buckets (second -> responses):")
        top = sorted(
            enumerate(per_second),
            key=lambda pair: pair[1],
            reverse=True,
        )[:print_top_seconds]
        for second, count in top:
            print(f"  +{second:>4}s -> {count}")


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Send synthetic traffic for a fixed period and report peak throughput. "
            "Defaults to POST /v1/log_event with synthetic payloads."
        )
    )
    parser.add_argument(
        "--target-url",
        default="http://127.0.0.1:8000/v1/log_event",
        help="Fully-qualified target URL.",
    )
    parser.add_argument(
        "--method",
        choices=("GET", "POST"),
        default="POST",
        help="HTTP method.",
    )
    parser.add_argument(
        "--duration-seconds",
        type=_parse_positive_float,
        default=60.0,
        help="Measurement duration in seconds.",
    )
    parser.add_argument(
        "--warmup-seconds",
        type=float,
        default=10.0,
        help="Warmup time before measurements begin.",
    )
    parser.add_argument(
        "--concurrency",
        type=_parse_positive_int,
        default=128,
        help="Number of parallel worker threads.",
    )
    parser.add_argument(
        "--timeout-seconds",
        type=_parse_positive_float,
        default=5.0,
        help="Request timeout per connection.",
    )
    parser.add_argument(
        "--insecure-https",
        action="store_true",
        help="Disable TLS certificate verification for HTTPS targets.",
    )
    parser.add_argument(
        "--sdk-key",
        default="secret-server-key",
        help="Value used for the statsig-api-key header.",
    )
    parser.add_argument(
        "--events-per-request",
        type=_parse_positive_int,
        default=50,
        help="Synthetic event count per POST request.",
    )
    parser.add_argument(
        "--duplicate-ratio",
        type=_parse_ratio,
        default=0.5,
        help="Fraction of synthetic events duplicated inside each request.",
    )
    parser.add_argument(
        "--body-pool-size",
        type=_parse_positive_int,
        default=4096,
        help="Number of synthetic POST bodies pre-generated and round-robined.",
    )
    parser.add_argument(
        "--body-file",
        type=Path,
        default=None,
        help="Optional file used as raw POST body. Overrides synthetic body generation.",
    )
    parser.add_argument(
        "--latency-sample-ratio",
        type=_parse_ratio,
        default=0.01,
        help="Fraction of requests sampled for latency percentiles.",
    )
    parser.add_argument(
        "--moving-average-window-seconds",
        type=_parse_positive_int,
        default=5,
        help="Window size used for the reported peak moving-average throughput.",
    )
    parser.add_argument(
        "--print-top-seconds",
        type=_parse_positive_int,
        default=10,
        help="How many top one-second throughput buckets to print.",
    )
    parser.add_argument(
        "--header",
        action="append",
        type=_parse_header,
        default=[],
        help="Additional request header (repeatable): --header 'Name: Value'",
    )
    args = parser.parse_args()

    if args.warmup_seconds < 0:
        raise SystemExit("--warmup-seconds must be >= 0")

    parsed = urlparse(args.target_url)
    if parsed.scheme not in ("http", "https"):
        raise SystemExit("target URL must start with http:// or https://")
    if not parsed.hostname:
        raise SystemExit("target URL must include a hostname")

    if args.body_file is not None and not args.body_file.exists():
        raise SystemExit(f"--body-file not found: {args.body_file}")

    path_query = parsed.path or "/"
    if parsed.query:
        path_query += f"?{parsed.query}"

    headers: Dict[str, str] = {
        "Connection": "keep-alive",
        "statsig-api-key": args.sdk_key,
    }
    if args.method == "POST":
        headers["Content-Type"] = "application/json"
    for key, value in args.header:
        headers[key] = value

    body_pool: List[bytes] = [b""]
    if args.method == "POST":
        body_pool = _build_body_pool(
            body_file=args.body_file,
            pool_size=args.body_pool_size,
            events_per_request=args.events_per_request,
            duplicate_ratio=args.duplicate_ratio,
        )

    print("Starting load test...")
    print(f"  target_url={args.target_url}")
    print(f"  method={args.method}")
    print(f"  concurrency={args.concurrency}")
    print(f"  warmup_seconds={args.warmup_seconds}")
    print(f"  duration_seconds={args.duration_seconds}")
    if args.method == "POST":
        print(f"  body_pool_size={len(body_pool)}")
        print(f"  events_per_request={args.events_per_request}")
        print(f"  duplicate_ratio={args.duplicate_ratio}")

    start = time.monotonic()
    warmup_ends_at = start + args.warmup_seconds
    test_start = warmup_ends_at
    test_ends_at = test_start + args.duration_seconds

    worker_results: List[WorkerResult] = [WorkerResult() for _ in range(args.concurrency)]
    threads = []
    for worker_id in range(args.concurrency):
        thread = threading.Thread(
            target=_worker_main,
            kwargs={
                "worker_id": worker_id,
                "parsed_url": parsed,
                "method": args.method,
                "path_query": path_query,
                "headers": headers,
                "body_pool": body_pool,
                "timeout_seconds": args.timeout_seconds,
                "insecure_https": args.insecure_https,
                "warmup_ends_at": warmup_ends_at,
                "test_ends_at": test_ends_at,
                "test_start": test_start,
                "latency_sample_ratio": args.latency_sample_ratio,
                "output": worker_results,
            },
            daemon=True,
        )
        threads.append(thread)
        thread.start()

    for thread in threads:
        thread.join()

    aggregated = AggregatedResult()
    for worker in worker_results:
        aggregated.merge(worker)

    _print_summary(
        result=aggregated,
        duration_seconds=args.duration_seconds,
        print_top_seconds=args.print_top_seconds,
        moving_average_window_seconds=args.moving_average_window_seconds,
    )

    if aggregated.responses == 0 and aggregated.transport_errors > 0:
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
