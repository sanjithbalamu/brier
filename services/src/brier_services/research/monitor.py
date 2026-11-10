"""Read-only, loopback-only view of the live maker's JSONL decision log."""

from __future__ import annotations

import contextlib
import json
import math
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

MAX_BYTES = 2_000_000


def latest(path: Path) -> dict[str, Any]:
    """Read a bounded tail, tolerating an incomplete final append without hiding old data."""
    if not path.exists():
        return {"markets": [], "read_at": time.time(), "status": "waiting for maker metrics"}
    with path.open("rb") as f:
        size = f.seek(0, 2)
        offset = max(0, size - MAX_BYTES)
        f.seek(offset)
        if offset:
            f.readline()
        lines = f.read(MAX_BYTES).splitlines()
    markets: dict[int, dict[str, Any]] = {}
    for line in lines:
        try:
            row = json.loads(line, parse_constant=lambda _: None)
            if (
                isinstance(row, dict)
                and isinstance(row.get("market"), int)
                and isinstance(row.get("t"), (int, float))
                and math.isfinite(row["t"])
            ):
                markets[row["market"]] = row
        except (ValueError, UnicodeDecodeError):
            continue
    return {
        "markets": sorted(markets.values(), key=lambda r: r["market"]),
        "read_at": time.time(),
        "status": "read-only local metrics",
    }


def handler(metrics: Path, port: int) -> type[BaseHTTPRequestHandler]:
    """Create a handler bound to one explicit metrics file; no filesystem URL mapping."""

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            if self.headers.get("Host") not in {f"127.0.0.1:{port}", f"localhost:{port}"}:
                self.send_error(403, "local host required")
                return
            path = urlsplit(self.path).path
            if path == "/api/metrics":
                body = json.dumps(latest(metrics), allow_nan=False).encode()
                content_type = "application/json"
            elif path == "/":
                body = Path(__file__).with_name("monitor.html").read_bytes()
                content_type = "text/html; charset=utf-8"
            else:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header(
                "Content-Security-Policy",
                "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; "
                "connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
            )
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, format: str, *args: Any) -> None:  # noqa: ANN401 - stdlib signature
            return

    return Handler


def serve(metrics: Path, port: int = 3010) -> None:
    """Expose no trading endpoint; bind exclusively to this machine's loopback interface."""
    if not 1024 <= port <= 65535:
        raise ValueError("monitor port must be 1024..65535")
    with ThreadingHTTPServer(("127.0.0.1", port), handler(metrics, port)) as server:
        print(f"Read-only bot monitor: http://127.0.0.1:{port}", flush=True)
        with contextlib.suppress(KeyboardInterrupt):
            server.serve_forever()
