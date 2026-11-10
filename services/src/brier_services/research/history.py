"""Record public market data and replay its visible state without inventing order fills."""

from __future__ import annotations

import asyncio
import datetime as dt
import json
import math
import time
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from websockets.asyncio.client import connect

from brier_services.client import Action, BookTracker, SequenceTracker
from brier_services.research.experiments import canonical, digest


async def capture(url: str, market: int, seconds: int, output: Path) -> None:
    """Subscribe anonymously and flush each received message. Reconnects require a new file."""
    parsed = urlsplit(url)
    if parsed.scheme not in {"ws", "wss"} or not parsed.hostname:
        raise ValueError("provide a ws:// or wss:// market-data endpoint")
    if parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise ValueError("recording endpoint must not contain credentials, query or fragment")
    if not 1 <= seconds <= 86400 or not 1 <= market <= 2**53 - 1:
        raise ValueError("seconds must be 1..86400 and market a positive safe integer")
    output.parent.mkdir(parents=True, exist_ok=True)
    book, trades = BookTracker(), SequenceTracker()
    with output.open("xb") as f:
        f.write(
            canonical(
                {
                    "schema": 1,
                    "kind": "observed public market data",
                    "market": market,
                    "source": url,
                    "started_at": dt.datetime.now(dt.UTC).isoformat(),
                }
            )
        )
        f.flush()
        start = time.monotonic()
        try:
            async with asyncio.timeout(seconds), connect(url) as ws:
                for channel in (f"book:{market}", f"trades:{market}"):
                    await ws.send(json.dumps({"op": "subscribe", "channel": channel}))
                async for raw in ws:
                    message = json.loads(raw)
                    channel = message.get("channel")
                    if channel not in {f"book:{market}", f"trades:{market}"}:
                        continue
                    f.write(canonical({"elapsed": time.monotonic() - start, "message": message}))
                    f.flush()
                    tracker = book if channel.startswith("book:") else trades
                    action = tracker.on_message(message)
                    if action is Action.RESUBSCRIBE:
                        await ws.send(json.dumps({"op": "unsubscribe", "channel": channel}))
                        await ws.send(json.dumps({"op": "subscribe", "channel": channel}))
        except TimeoutError:
            pass


def read_capture(path: Path) -> dict[str, Any]:
    """Rebuild only sequence-complete books; lost messages are explicit gaps."""
    raw = path.read_bytes()
    lines = raw.splitlines()
    if not lines:
        raise ValueError("empty market recording")
    header = json.loads(lines[0])
    if header.get("schema") != 1 or header.get("kind") != "observed public market data":
        raise ValueError("unsupported recording format")
    market = header["market"]
    book, trades = BookTracker(), SequenceTracker()
    frames = []
    previous = 0.0
    for line in lines[1:]:
        row = json.loads(line)  # A truncated recording must be repaired explicitly, not hidden.
        elapsed, message = row["elapsed"], row["message"]
        if not math.isfinite(elapsed) or elapsed < previous:
            raise ValueError("recording timestamps must be finite and monotonic")
        previous = elapsed
        channel = message["channel"]
        trade = None
        if channel == f"book:{market}":
            action = book.on_message(message)
        elif channel == f"trades:{market}":
            action = trades.on_message(message)
            if action is Action.APPLIED and message["type"] == "trade":
                trade = message
        else:
            raise ValueError("recording contains a different or private channel")
        frames.append(
            {
                "elapsed": elapsed,
                "action": action.value,
                "synced": book.synced,
                "state": book.state,
                "gaps": book.gaps + trades.gaps,
                "bids": sorted(book.bids.items(), reverse=True) if book.synced else [],
                "asks": sorted(book.asks.items()) if book.synced else [],
                "trade": trade,
            }
        )
    if not frames:
        raise ValueError("recording has no public market messages")
    return {"header": header, "sha256": digest(raw), "frames": frames}


def render_history(path: Path, output: Path) -> None:
    """Write a portable viewer; recorded trades are observations, never hypothetical fills."""
    payload = read_capture(path)
    data = json.dumps(payload, allow_nan=False).replace("<", "\\u003c").replace("&", "\\u0026")
    template = Path(__file__).with_name("history.html").read_text()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(template.replace("__HISTORY_DATA__", data))
