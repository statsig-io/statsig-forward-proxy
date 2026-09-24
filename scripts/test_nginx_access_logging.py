#!/usr/bin/env python3
"""Exercise the real entrypoint and all nginx templates with synthetic credentials.

Requires nginx and openssl on PATH. Only filesystem paths and listening/upstream
ports are changed in temporary copies; production rendering and launch logic run
unchanged. Run as a non-root user to check unprivileged stdout access.
"""

import http.client
import os
from pathlib import Path
import shutil
import signal
import socket
import ssl
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = Path(__file__).resolve().parents[1]
SECRET = "client-synthetic-secret"
CASES = [
    (f"/v2/download_config_specs/{SECRET}.json?k={SECRET}", "/v2/download_config_specs/[REDACTED]"),
    (f"/v1/download_config_specs/{SECRET}?k=one&k={SECRET}", "/v1/download_config_specs/[REDACTED]"),
    (f"/v2/download_config_specs_deltas/{SECRET}", "/v2/download_config_specs_deltas/[REDACTED]"),
    (f"/v2/download_config_specs/%63lient-synthetic-secret/extra", "/v2/download_config_specs/[REDACTED]"),
    (f"/v1/download_id_list_file/{SECRET}", "/v1/download_id_list_file/[REDACTED]"),
    (f"/v1/get_id_lists?%6b={SECRET}&K={SECRET}", "/v1/get_id_lists"),
    (f"/v1/log_event?k={SECRET}", "/v1/log_event"),
    (f"/v1/health?k={SECRET}", "/v1/health"),
    (f"/upstream-error/{SECRET}?k={SECRET}", "[REDACTED]"),
    (f"/v2/download_config_specs/../{SECRET}", "[REDACTED]"),
    (f"/unknown/{SECRET}?k={SECRET}", "[REDACTED]"),
    (f"/v2/download_config_specs{SECRET}", "[REDACTED]"),
]


class Upstream(BaseHTTPRequestHandler):
    seen = []

    def do_GET(self):
        self.seen.append((self.path, self.headers.get("statsig-api-key")))
        if self.path.startswith("/upstream-error/"):
            self.close_connection = True
            return
        self.send_response(404 if self.path.startswith("/unknown/") else 200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *args):
        pass


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for_port(port, process):
    for _ in range(100):
        if process.poll() is not None:
            raise AssertionError("entrypoint exited before nginx started")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                return
        except OSError:
            time.sleep(0.05)
    raise AssertionError("nginx did not start")


def run_mode(mode, upstream_port, nginx):
    with tempfile.TemporaryDirectory(prefix="sfp-access-log-") as tmp:
        work = Path(tmp)
        http_port, https_port = free_port(), free_port()
        cert, key = work / "cert.pem", work / "key.pem"
        subprocess.run([
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", str(key), "-out", str(cert), "-days", "1", "-subj", "/CN=localhost",
        ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for source in ROOT.glob("nginx-*.conf.template"):
            template = source.read_text().replace("listen 8000;", f"listen {http_port};")
            template = template.replace("listen 8443 ssl;", f"listen {https_port} ssl;")
            template = template.replace("server 127.0.0.1:8080;", f"server 127.0.0.1:{upstream_port};")
            # Keep every temporary file under the unprivileged test directory.
            template = template.replace("http {", "http {\n" + "\n".join(
                f"    {name}_temp_path {work / name};"
                for name in ("client_body", "proxy", "fastcgi", "uwsgi", "scgi")
            ))
            (work / source.name).write_text(template)
        entrypoint = (ROOT / "entrypoint.sh").read_text()
        entrypoint = entrypoint.replace('"/nginx-', f'"{work}/nginx-')
        entrypoint = entrypoint.replace("/tmp/nginx.conf", str(work / "nginx.conf"))
        entrypoint = entrypoint.replace("/tmp/nginx.pid", str(work / "nginx.pid"))
        (work / "entrypoint.sh").write_text(entrypoint)
        # Substitute only the Rust executable; nginx itself is real.
        bin_dir = work / "bin"
        bin_dir.mkdir()
        (bin_dir / "statsig_forward_proxy").write_text("#!/bin/sh\nexec sleep 60\n")
        (bin_dir / "statsig_forward_proxy").chmod(0o755)
        (bin_dir / "nginx").write_text(f'#!/bin/sh\nexec "{nginx}" -e /dev/stderr -p "{work}" "$@"\n')
        (bin_dir / "nginx").chmod(0o755)
        env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}",
                   PROXY_CACHE_PATH_CONFIGURATION=str(work / "cache"), NGINX_WORKER_PROCESSES="1")
        # Avoid inheriting caller-specific cache paths into the test fixture.
        env["PROXY_CACHE_DOWNLOAD_PATH_CONFIGURATION"] = str(work / "cache/download")
        env["PROXY_CACHE_DOWNLOAD_ID_LIST_PATH_CONFIGURATION"] = str(work / "cache/id-list")
        args = ["sh", str(work / "entrypoint.sh")]
        if mode != "http-only":
            args += ["--x509-server-cert-path", str(cert), "--x509-server-key-path", str(key),
                     "--x509-client-cert-path", str(cert)]
        if mode == "https-only":
            args += ["--enforce-tls"]
        stdout, stderr = work / "stdout", work / "stderr"
        with stdout.open("w") as out, stderr.open("w") as err:
            proc = subprocess.Popen(args, env=env, stdout=out, stderr=err, start_new_session=True)
            try:
                listeners = []
                if mode != "https-only":
                    listeners.append((http_port, False))
                if mode != "http-only":
                    listeners.append((https_port, True))
                for port, tls in listeners:
                    wait_for_port(port, proc)
                    for path, safe_path in CASES:
                        before = len(stdout.read_text().splitlines())
                        if tls:
                            conn = http.client.HTTPSConnection("127.0.0.1", port, timeout=5,
                                                               context=ssl._create_unverified_context())
                        else:
                            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                        conn.request("GET", path, headers={
                            "statsig-api-key": SECRET, "Referer": f"https://example.test/?k={SECRET}",
                            "User-Agent": SECRET, "Authorization": f"Bearer {SECRET}",
                        })
                        response = conn.getresponse()
                        status = response.status
                        expected = 502 if path.startswith("/upstream-error/") else (
                            404 if path.startswith("/unknown/") else 200)
                        assert status == expected, status
                        body = response.read()
                        if status != 502:
                            assert body == b"ok"
                        conn.close()
                        for _ in range(100):
                            lines = stdout.read_text().splitlines()
                            if len(lines) > before:
                                break
                            time.sleep(0.01)
                        assert len(lines) == before + 1, (path, lines)
                        assert f'"GET {safe_path} HTTP/1.1" {status} {len(body)} "-" "-"' in lines[-1], lines[-1]
                        assert SECRET not in stdout.read_text() + stderr.read_text()
                        assert "synthetic-secret" not in stdout.read_text()
                        assert "?" not in lines[-1], lines[-1]
                assert (work / "nginx.pid").exists(), "nginx master did not stay running"
                print(f"PASS {mode}: {len(listeners) * len(CASES)} requests via entrypoint stdout (uid={os.getuid()})")
            except Exception:
                print(stderr.read_text())
                raise
            finally:
                pid_file = work / "nginx.pid"
                if pid_file.exists():
                    try:
                        os.kill(int(pid_file.read_text()), signal.SIGQUIT)
                    except ProcessLookupError:
                        pass
                proc.terminate()
                proc.wait(timeout=5)
                # Wait for nginx to finish closing its cache files before cleanup.
                for _ in range(100):
                    if not pid_file.exists():
                        break
                    time.sleep(0.05)
                assert not pid_file.exists(), "nginx did not shut down"


def main():
    nginx = shutil.which("nginx")
    if not nginx or not shutil.which("openssl"):
        raise SystemExit("nginx and openssl must be installed and on PATH")
    server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        for mode in ("http-only", "https-only", "http-https"):
            run_mode(mode, server.server_port, nginx)
        for path, _ in CASES:
            assert (path, SECRET) in Upstream.seen, f"request changed before upstream: {path}"
        print("PASS original paths, query strings, and SDK-key headers reach upstream unchanged")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    main()
