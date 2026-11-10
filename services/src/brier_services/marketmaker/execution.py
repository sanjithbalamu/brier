"""Persist an order intent before sending it; resolve uncertainty before sending another."""

from __future__ import annotations

import json
import secrets
import sqlite3
from pathlib import Path
from typing import Any, Protocol

from brier_services.client import ApiError


class Api(Protocol):
    def request(self, method: str, path: str, body: dict[str, Any] | None = None) -> Any: ...  # noqa: ANN401


class OrderExecutor:
    """One dedicated maker account, one persistent intent and one process per state file.

    A transport failure leaves the exact body on disk. Retrying that client_order_id either
    returns the original order or places it once. No later order may evict its idempotency key.
    Server-side cancellation leases bound resting-quote lifetime while the client is offline.
    """

    def __init__(self, api: Api, path: Path | None = None, identity: str | None = None) -> None:
        self.api = api
        self.lock: sqlite3.Connection | None = None
        if path is not None:
            path.parent.mkdir(parents=True, exist_ok=True)
            self.lock = sqlite3.connect(str(path) + ".lock", timeout=0)
            self.lock.execute("BEGIN EXCLUSIVE")
        self.db = sqlite3.connect(str(path) if path is not None else ":memory:")
        self.db.execute("PRAGMA synchronous=FULL")
        self.db.execute(
            "CREATE TABLE IF NOT EXISTS intent "
            "(id INTEGER PRIMARY KEY CHECK(id=1), body TEXT NOT NULL)"
        )
        self.db.execute("CREATE TABLE IF NOT EXISTS identity (name TEXT PRIMARY KEY)")
        if identity is not None:
            stored = self.db.execute("SELECT name FROM identity").fetchone()
            if stored is not None and stored[0] != identity:
                self.close()
                raise ValueError("order state belongs to another gateway/account")
            self.db.execute("INSERT OR IGNORE INTO identity(name) VALUES(?)", (identity,))
        self.db.commit()

    def close(self) -> None:
        """Close the state store; unresolved intents remain recoverable."""
        self.db.close()
        if self.lock is not None:
            self.lock.close()

    def resolve(self) -> dict[str, Any] | None:
        """Retry the exact pending intent; propagate uncertainty without deleting it."""
        row = self.db.execute("SELECT body FROM intent WHERE id=1").fetchone()
        if row is None:
            return None
        body = json.loads(row[0])
        try:
            result = self.api.request("POST", "/v1/orders", body)
        except ApiError as e:
            # Sequenced rejection means the engine definitively did not accept this intent.
            # A non-sequenced auth/rate/transport error cannot resolve a previous timeout.
            if e.seq is not None:
                self._clear()
            raise
        if (
            not isinstance(result, dict)
            or type(result.get("order")) is not int
            or result["order"] <= 0
        ):
            raise ValueError("order response lacks an authoritative order id; intent retained")
        self._clear()
        return dict(result)

    def _clear(self) -> None:
        with self.db:
            self.db.execute("DELETE FROM intent WHERE id=1")

    def place(self, body: dict[str, Any]) -> dict[str, Any]:
        """Store the intent durably before the first network write."""
        if self.db.execute("SELECT 1 FROM intent").fetchone() is not None:
            raise RuntimeError("resolve the pending order and refresh portfolio before placing")
        intent = {**body, "client_order_id": secrets.randbits(53), "post_only": True, "tif": "gtc"}
        with self.db:
            self.db.execute("INSERT INTO intent(id, body) VALUES(1, ?)", (json.dumps(intent),))
        result = self.resolve()
        if result is None:
            raise RuntimeError("persisted intent disappeared")
        return result
