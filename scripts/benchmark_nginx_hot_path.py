#!/usr/bin/env python3
"""Benchmark nginx hot-path throughput with a local mock upstream.

The script:
1. Renders nginx-http-only.conf.template with cache placeholders resolved.
2. Starts a mock upstream on 127.0.0.1:8080 that returns a static payload.
3. Starts nginx with the rendered config.
4. Sweeps a set of concurrency levels using ApacheBench (ab) and reports QPS.
"""

from __future__ import annotations

import argparse
import contextlib
import http.client
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Dict, List, Optional
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


def _parse_concurrency_list(value: str) -> List[int]:
    try:
        parsed = [int(v.strip()) for v in value.split(",") if v.strip()]
    except ValueError as err:
        raise argparse.ArgumentTypeError(f"invalid concurrency list: {value}") from err
    if not parsed or any(v <= 0 for v in parsed):
        raise argparse.ArgumentTypeError("concurrency list must be comma-separated positive ints")
    return parsed


def _format_duration(seconds: float) -> str:
    if seconds < 1:
        return f"{seconds * 1000:.0f}ms"
    return f"{seconds:.2f}s"


@dataclass
class BenchResult:
    concurrency: int
    complete_requests: int
    failed_requests: int
    non_2xx_responses: int
    requests_per_sec: float
    transfer_kbps: float
    p50_ms: Optional[float]
    p95_ms: Optional[float]
    p99_ms: Optional[float]
    upstream_hits_delta: int

    @property
    def estimated_cache_hit_ratio(self) -> Optional[float]:
        if self.complete_requests <= 0:
            return None
        ratio = 1.0 - (self.upstream_hits_delta / self.complete_requests)
        return max(0.0, min(1.0, ratio))


class MockState:
    def __init__(self, payload: bytes) -> None:
        self.payload = payload
        self._lock = threading.Lock()
        self._upstream_hit_count = 0

    def increment(self) -> None:
        with self._lock:
            self._upstream_hit_count += 1

    def read_hits(self) -> int:
        with self._lock:
            return self._upstream_hit_count


class MockHandler(BaseHTTPRequestHandler):
    state: MockState

    def log_message(self, fmt: str, *args: object) -> None:
        return

    def do_GET(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        if not (parsed.path.startswith("/v1/download_config_specs/") or parsed.path.startswith("/v2/download_config_specs/")):
            self.send_response(404)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"not found")
            return

        self.state.increment()
        payload = self.state.payload
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Cache-Control", "max-age=300")
        self.send_header("X-Since-Time", "1")
        self.end_headers()
        self.wfile.write(payload)


def _build_payload(payload_size_mb: float) -> bytes:
    payload_bytes = int(payload_size_mb * 1024 * 1024)
    prefix = b'{"configs":"'
    suffix = b'"}'
    fill_bytes = payload_bytes - len(prefix) - len(suffix)
    if fill_bytes <= 0:
        raise ValueError("payload-size-mb is too small to produce valid JSON payload")
    return prefix + (b"x" * fill_bytes) + suffix


def _render_nginx_config(
    template_path: Path,
    output_path: Path,
    cache_dir: Path,
    cache_ttl: str,
    mock_port: int,
    nginx_port: int,
) -> None:
    template = template_path.read_text(encoding="utf-8")
    substitutions: Dict[str, str] = {
        "PROXY_CACHE_PATH_CONFIGURATION": str(cache_dir),
        "PROXY_CACHE_MAX_SIZE_IN_MB": "1024",
        "PROXY_CACHE_TTL": cache_ttl,
        "PROXY_CACHE_CLEANUP_MAX_DURATION_MS": "200",
        "PROXY_CACHE_CLEANUP_SLEEP_INTERVAL_MS": "50",
        "PROXY_CACHE_CLEANUP_MAX_FILES_DELETED_PER_INTERVAL": "100",
    }
    rendered = template
    for key, value in substitutions.items():
        rendered = rendered.replace(f"{{{{{key}}}}}", value)

    # Keep template semantics, but allow local benchmark ports.
    rendered = rendered.replace("server 127.0.0.1:8080;", f"server 127.0.0.1:{mock_port};")
    rendered = rendered.replace("listen 8000 reuseport;", f"listen {nginx_port} reuseport;")
    output_path.write_text(rendered, encoding="utf-8")


def _wait_for_http_ready(url: str, timeout_s: float) -> None:
    parsed = urlparse(url)
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        try:
            conn = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=1.0)
            conn.request("GET", parsed.path + (f"?{parsed.query}" if parsed.query else ""))
            response = conn.getresponse()
            _ = response.read()
            if 200 <= response.status < 500:
                return
        except OSError:
            time.sleep(0.1)
        finally:
            with contextlib.suppress(Exception):
                conn.close()  # type: ignore[misc]
    raise TimeoutError(f"timed out waiting for {url} to become ready")


def _parse_ab_metrics(raw_output: str) -> Dict[str, Optional[float]]:
    def extract_float(pattern: str) -> Optional[float]:
        match = re.search(pattern, raw_output, flags=re.MULTILINE)
        if not match:
            return None
        return float(match.group(1))

    def extract_int(pattern: str) -> int:
        match = re.search(pattern, raw_output, flags=re.MULTILINE)
        if not match:
            return 0
        return int(match.group(1))

    percentile_matches = re.findall(r"^\s*(\d+)%\s+([0-9.]+)\s*$", raw_output, flags=re.MULTILINE)
    percentiles: Dict[int, float] = {}
    for pct, val in percentile_matches:
        percentiles[int(pct)] = float(val)

    return {
        "complete_requests": float(extract_int(r"^Complete requests:\s+(\d+)\s*$")),
        "failed_requests": float(extract_int(r"^Failed requests:\s+(\d+)\s*$")),
        "non_2xx_responses": float(extract_int(r"^Non-2xx responses:\s+(\d+)\s*$")),
        "requests_per_sec": extract_float(r"^Requests per second:\s+([0-9.]+)\s+\[#/sec\]"),
        "transfer_kbps": extract_float(r"^Transfer rate:\s+([0-9.]+)\s+\[Kbytes/sec\]"),
        "p50_ms": percentiles.get(50),
        "p95_ms": percentiles.get(95),
        "p99_ms": percentiles.get(99),
    }


def _run_ab(
    ab_bin: str,
    url: str,
    requests: int,
    concurrency: int,
    timeout_seconds: int,
    accept_encoding: str,
) -> str:
    command = [
        ab_bin,
        "-k",
        "-n",
        str(requests),
        "-c",
        str(concurrency),
        "-s",
        str(timeout_seconds),
        "-H",
        f"Accept-Encoding: {accept_encoding}",
        url,
    ]
    completed = subprocess.run(command, capture_output=True, text=True, check=False)
    output = completed.stdout + "\n" + completed.stderr
    if completed.returncode != 0:
        raise RuntimeError(
            "ab command failed.\n"
            f"command: {' '.join(shlex.quote(c) for c in command)}\n"
            f"output:\n{output}"
        )
    return output


def _single_http_get(url: str, timeout_s: float) -> int:
    parsed = urlparse(url)
    conn = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=timeout_s)
    try:
        conn.request("GET", parsed.path + (f"?{parsed.query}" if parsed.query else ""))
        response = conn.getresponse()
        _ = response.read()
        return response.status
    finally:
        conn.close()


def _start_mock_server(port: int, payload: bytes) -> tuple[ThreadingHTTPServer, threading.Thread, MockState]:
    state = MockState(payload)
    handler_type = type("HotPathMockHandler", (MockHandler,), {"state": state})
    server = ThreadingHTTPServer(("127.0.0.1", port), handler_type)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, thread, state


def _print_summary(results: List[BenchResult], elapsed_s: float, payload_size_mb: float) -> None:
    if not results:
        print("No benchmark results produced.")
        return

    headers = [
        "conc",
        "req/s",
        "p50(ms)",
        "p95(ms)",
        "p99(ms)",
        "failed",
        "non2xx",
        "upstream_hits",
        "est_cache_hit",
    ]
    print(" ".join(h.rjust(12) for h in headers))
    for result in results:
        hit_ratio = result.estimated_cache_hit_ratio
        row = [
            f"{result.concurrency}",
            f"{result.requests_per_sec:.2f}",
            f"{result.p50_ms:.2f}" if result.p50_ms is not None else "n/a",
            f"{result.p95_ms:.2f}" if result.p95_ms is not None else "n/a",
            f"{result.p99_ms:.2f}" if result.p99_ms is not None else "n/a",
            f"{result.failed_requests}",
            f"{result.non_2xx_responses}",
            f"{result.upstream_hits_delta}",
            f"{(hit_ratio * 100):.2f}%" if hit_ratio is not None else "n/a",
        ]
        print(" ".join(c.rjust(12) for c in row))

    best = max(results, key=lambda r: r.requests_per_sec)
    print("")
    print(f"Best observed throughput: {best.requests_per_sec:.2f} req/s at concurrency={best.concurrency}")
    print(f"Payload size: {payload_size_mb:.2f}MB")
    print(f"Total benchmark wall time: {_format_duration(elapsed_s)}")


def main() -> int:
    script_path = Path(__file__).resolve()
    repo_root = script_path.parent.parent
    default_template = repo_root / "nginx-http-only.conf.template"

    parser = argparse.ArgumentParser(
        description="Benchmark nginx hot path with a local mock upstream returning a static payload.",
        formatter_class=argparse.RawTextHelpFormatter,
        epilog=textwrap.dedent(
            """\
            Example:
              ./scripts/benchmark_nginx_hot_path.py \
                --payload-size-mb 5 \
                --concurrency 16,32,64,128 \
                --requests-per-run 20000
            """
        ),
    )
    parser.add_argument("--template", type=Path, default=default_template, help="Path to nginx template file.")
    parser.add_argument("--nginx-bin", default=(shutil.which("nginx") or "nginx"), help="Path to nginx binary.")
    parser.add_argument("--ab-bin", default=(shutil.which("ab") or "ab"), help="Path to ApacheBench binary.")
    parser.add_argument("--payload-size-mb", type=_parse_positive_float, default=5.0, help="Mock payload size in MB.")
    parser.add_argument("--mock-port", type=_parse_positive_int, default=8080, help="Mock upstream port.")
    parser.add_argument("--nginx-port", type=_parse_positive_int, default=8000, help="nginx listen port.")
    parser.add_argument(
        "--target-path",
        default="/v1/download_config_specs/secret-client-key.json?sinceTime=0&supports_proto=1&accept_deltas=0",
        help="HTTP path to benchmark through nginx.",
    )
    parser.add_argument("--cache-ttl", default="5s", help="Value used for PROXY_CACHE_TTL substitution.")
    parser.add_argument(
        "--cache-dir",
        type=Path,
        default=None,
        help=(
            "Cache directory for nginx proxy_cache_path. "
            "Defaults to /dev/shm/nginx-hot-path-bench-<pid> when available."
        ),
    )
    parser.add_argument(
        "--concurrency",
        type=_parse_concurrency_list,
        default=[16, 32, 64],
        help="Comma-separated concurrency sweep, e.g. 16,32,64,128.",
    )
    parser.add_argument("--requests-per-run", type=_parse_positive_int, default=1000, help="Total requests for each ab run.")
    parser.add_argument("--timeout-seconds", type=_parse_positive_int, default=15, help="ab socket timeout in seconds.")
    parser.add_argument(
        "--accept-encoding",
        default="identity",
        help="Accept-Encoding header value used by ab. Defaults to identity for a stable cache key.",
    )
    args = parser.parse_args()

    if not args.template.exists():
        print(f"Template not found: {args.template}", file=sys.stderr)
        return 1

    if not shutil.which(args.nginx_bin) and not Path(args.nginx_bin).exists():
        print(f"nginx binary not found: {args.nginx_bin}", file=sys.stderr)
        return 1
    if not shutil.which(args.ab_bin) and not Path(args.ab_bin).exists():
        print(f"ab binary not found: {args.ab_bin}", file=sys.stderr)
        return 1

    if not args.target_path.startswith("/"):
        print("--target-path must start with '/'", file=sys.stderr)
        return 1

    payload = _build_payload(args.payload_size_mb)

    start_ts = time.time()
    nginx_proc: Optional[subprocess.Popen[str]] = None
    mock_server: Optional[ThreadingHTTPServer] = None
    mock_thread: Optional[threading.Thread] = None

    with tempfile.TemporaryDirectory(prefix="nginx-hot-path-bench-") as tmpdir_name:
        tmpdir = Path(tmpdir_name)
        cache_dir_needs_cleanup = False
        if args.cache_dir is not None:
            requested_cache_dir = args.cache_dir.expanduser()
            try:
                requested_cache_dir.mkdir(parents=True, exist_ok=True)
                cache_dir = requested_cache_dir
            except OSError as err:
                # macOS commonly has no writable /dev/shm; keep benchmarks usable.
                fallback_cache_dir = tmpdir / "cache"
                fallback_cache_dir.mkdir(parents=True, exist_ok=True)
                cache_dir = fallback_cache_dir
                print(
                    "Warning: unable to use requested --cache-dir "
                    f"'{requested_cache_dir}' ({err}). Falling back to '{cache_dir}'.",
                    file=sys.stderr,
                )
        else:
            shm_root = Path("/dev/shm")
            if shm_root.exists() and os.access(shm_root, os.W_OK | os.X_OK):
                cache_dir = shm_root / f"nginx-hot-path-bench-{os.getpid()}"
                try:
                    cache_dir.mkdir(parents=True, exist_ok=True)
                    cache_dir_needs_cleanup = True
                except OSError as err:
                    fallback_cache_dir = tmpdir / "cache"
                    fallback_cache_dir.mkdir(parents=True, exist_ok=True)
                    cache_dir = fallback_cache_dir
                    print(
                        "Warning: unable to create default /dev/shm cache dir "
                        f"({err}). Falling back to '{cache_dir}'.",
                        file=sys.stderr,
                    )
            else:
                cache_dir = tmpdir / "cache"
                cache_dir.mkdir(parents=True, exist_ok=True)
        nginx_conf = tmpdir / "nginx.conf"
        nginx_pid = tmpdir / "nginx.pid"

        print(f"Using nginx cache directory: {cache_dir}")

        _render_nginx_config(
            template_path=args.template,
            output_path=nginx_conf,
            cache_dir=cache_dir,
            cache_ttl=args.cache_ttl,
            mock_port=args.mock_port,
            nginx_port=args.nginx_port,
        )

        mock_server, mock_thread, mock_state = _start_mock_server(args.mock_port, payload)

        nginx_cmd = [
            args.nginx_bin,
            "-g",
            f"pid {nginx_pid}; daemon off;",
            "-c",
            str(nginx_conf),
        ]
        nginx_proc = subprocess.Popen(nginx_cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

        url = f"http://127.0.0.1:{args.nginx_port}{args.target_path}"
        try:
            _wait_for_http_ready(url, timeout_s=10.0)
        except Exception:
            stderr = ""
            if nginx_proc and nginx_proc.stderr:
                with contextlib.suppress(Exception):
                    stderr = nginx_proc.stderr.read()
            print("nginx failed to become ready.", file=sys.stderr)
            if stderr:
                print(stderr, file=sys.stderr)
            return 1

        warm_status = _single_http_get(url, timeout_s=5.0)
        if warm_status != 200:
            print(f"Warmup request returned non-200 status: {warm_status}", file=sys.stderr)
            return 1

        results: List[BenchResult] = []
        try:
            for concurrency in args.concurrency:
                before_hits = mock_state.read_hits()
                raw_ab = _run_ab(
                    ab_bin=args.ab_bin,
                    url=url,
                    requests=args.requests_per_run,
                    concurrency=concurrency,
                    timeout_seconds=args.timeout_seconds,
                    accept_encoding=args.accept_encoding,
                )
                after_hits = mock_state.read_hits()
                parsed = _parse_ab_metrics(raw_ab)

                req_per_sec = parsed["requests_per_sec"] if parsed["requests_per_sec"] is not None else 0.0
                transfer_kbps = parsed["transfer_kbps"] if parsed["transfer_kbps"] is not None else 0.0
                complete_requests = int(parsed["complete_requests"] or 0)
                failed_requests = int(parsed["failed_requests"] or 0)
                non_2xx = int(parsed["non_2xx_responses"] or 0)

                results.append(
                    BenchResult(
                        concurrency=concurrency,
                        complete_requests=complete_requests,
                        failed_requests=failed_requests,
                        non_2xx_responses=non_2xx,
                        requests_per_sec=req_per_sec,
                        transfer_kbps=transfer_kbps,
                        p50_ms=parsed["p50_ms"],
                        p95_ms=parsed["p95_ms"],
                        p99_ms=parsed["p99_ms"],
                        upstream_hits_delta=(after_hits - before_hits),
                    )
                )
        finally:
            if nginx_proc:
                with contextlib.suppress(Exception):
                    nginx_proc.send_signal(signal.SIGTERM)
                with contextlib.suppress(Exception):
                    nginx_proc.wait(timeout=5)
                if nginx_proc.poll() is None:
                    with contextlib.suppress(Exception):
                        nginx_proc.kill()

            if mock_server:
                with contextlib.suppress(Exception):
                    mock_server.shutdown()
                with contextlib.suppress(Exception):
                    mock_server.server_close()
            if mock_thread:
                with contextlib.suppress(Exception):
                    mock_thread.join(timeout=2)
            if cache_dir_needs_cleanup:
                with contextlib.suppress(Exception):
                    shutil.rmtree(cache_dir)

    elapsed = time.time() - start_ts
    _print_summary(results, elapsed_s=elapsed, payload_size_mb=args.payload_size_mb)

    if any(r.failed_requests > 0 or r.non_2xx_responses > 0 for r in results):
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
