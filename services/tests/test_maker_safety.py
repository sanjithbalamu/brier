"""Lost acknowledgements, restart recovery, freshness and correlated exposure limits."""

from __future__ import annotations

import sqlite3
from pathlib import Path
from typing import Any

import pytest

from brier_services.marketmaker.execution import OrderExecutor
from brier_services.marketmaker.risk import Limits, allowed_quantity, exposure
from brier_services.marketmaker.service import FairValues, StaleError
from brier_services.resolution.fetch import FetchError


class LostReply:
    def __init__(self) -> None:
        self.accepted: dict[int, dict[str, Any]] = {}

    def request(self, method: str, path: str, body: dict[str, Any] | None = None) -> Any:  # noqa: ANN401
        assert body is not None
        key = body["client_order_id"]
        if key in self.accepted:
            assert body == self.accepted[key]
            return {"status": "duplicate", "order": 123}
        self.accepted[key] = body.copy()
        raise TimeoutError("accepted but connection failed before reply")


def test_unknown_order_is_reconciled_after_process_restart(tmp_path: Path) -> None:
    api = LostReply()
    state = tmp_path / "orders.sqlite"
    first = OrderExecutor(api, state)
    order = {"market": 1, "side": "buy", "price": 50, "qty": 5}
    with pytest.raises(TimeoutError):
        first.place(order)
    with pytest.raises(RuntimeError, match="pending"):
        first.place(order)
    first.close()
    restarted = OrderExecutor(api, state)
    assert restarted.resolve() == {"status": "duplicate", "order": 123}
    assert restarted.resolve() is None
    assert len(api.accepted) == 1
    assert next(iter(api.accepted.values()))["post_only"] is True
    restarted.close()


def test_risk_counts_both_sides_and_related_outcomes() -> None:
    rules = {i: {"event": {"slug": "same-day", "category": "weather"}} for i in (1, 2)}
    portfolio = {
        "positions": [{"market": 1, "position": 70}],
        "open_orders": [
            {"market": 1, "side": "buy", "remaining": 20},
            {"market": 1, "side": "sell", "remaining": 30},
        ],
    }
    assert exposure(portfolio) == {1: 90}
    limits = Limits(market=100, event=100, category=200, total=300)
    assert allowed_quantity(portfolio, rules, limits, 2, "buy", 25) == 10
    assert allowed_quantity(portfolio, rules, limits, 1, "buy", 25) == 10
    assert allowed_quantity(portfolio, rules, limits, 1, "sell", 25) == 25


def test_account_limit_and_short_positions_are_conservative() -> None:
    portfolio = {"positions": [{"market": 1, "position": -80}], "open_orders": []}
    limits = Limits(market=50, event=50, category=50, total=50)
    assert allowed_quantity(portfolio, {}, limits, 1, "sell", 10) == 0
    assert allowed_quantity(portfolio, {}, limits, 1, "buy", 10) == 10
    assert allowed_quantity(portfolio, {}, limits, 2, "buy", 10) == 0


def test_source_specific_freshness_and_no_future_cache() -> None:
    clock = [100.0]
    fair = FairValues(None, wall=lambda: clock[0])
    assert fair._get("OPEN-METEO NYC", lambda: b"forecast") == b"forecast"
    clock[0] += 1900

    def offline() -> bytes:
        raise OSError("offline")

    assert fair._get("OPEN-METEO NYC", offline) == b"forecast"
    clock[0] += 6000
    with pytest.raises(StaleError, match="unavailable"):
        fair._get("OPEN-METEO NYC", offline)
    clock[0] = 0
    with pytest.raises(StaleError, match="future"):
        fair._get("OPEN-METEO NYC", offline)


def test_public_failure_reason_does_not_copy_credential_bearing_urls() -> None:
    fair = FairValues(None)

    def unavailable() -> bytes:
        raise FetchError("https://example.test/forecast?api_key=private-test-value")

    with pytest.raises(StaleError, match="FetchError") as error:
        fair._get("DGS10", unavailable)
    assert "private-test-value" not in str(error.value)
    assert "https://" not in str(error.value)


def test_order_state_is_single_process_and_account_bound(tmp_path: Path) -> None:
    state = tmp_path / "orders.sqlite"
    first = OrderExecutor(LostReply(), state, "gateway:account1")
    with pytest.raises(sqlite3.OperationalError, match="locked"):
        OrderExecutor(LostReply(), state, "gateway:account1")
    first.close()
    with pytest.raises(ValueError, match="another"):
        OrderExecutor(LostReply(), state, "gateway:account2")
    reopened = OrderExecutor(LostReply(), state, "gateway:account1")
    reopened.close()
