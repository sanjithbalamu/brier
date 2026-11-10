"""Conservative exposure budgets including every possible fill of resting orders.

Budgets are contract-equivalents at maximum payout, not estimated P&L or value-at-risk. No
offsetting credit is taken for supposedly correlated markets; event/category aggregation
therefore remains conservative without inventing a joint probability model.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class Limits:
    market: int = 500
    event: int = 1000
    category: int = 2500
    total: int = 5000

    def __post_init__(self) -> None:
        if min(self.market, self.event, self.category, self.total) < 1:
            raise ValueError("exposure budgets must be positive")


def exposure(portfolio: dict[str, Any]) -> dict[int, int]:
    """Largest absolute position after any subset of resting bids/offers has filled."""
    positions = {int(p["market"]): int(p["position"]) for p in portfolio["positions"]}
    ranges = {m: [p, p] for m, p in positions.items()}
    for order in portfolio["open_orders"]:
        market = int(order["market"])
        bounds = ranges.setdefault(market, [0, 0])
        if order["side"] == "buy":
            bounds[1] += int(order["remaining"])
        else:
            bounds[0] -= int(order["remaining"])
    return {m: max(abs(low), abs(high)) for m, (low, high) in ranges.items()}


def totals(
    amounts: dict[int, int], rules: dict[int, dict[str, Any]], market: int
) -> tuple[int, int, int, int]:
    """Market, event, category, account exposures relevant to a candidate order."""

    def group(m: int) -> tuple[str, str]:
        rule = rules.get(m, {})
        event = rule.get("event") or {}
        provider = rule.get("source", {}).get("provider", "unknown")
        category = {
            "nws": "weather",
            "noaa": "weather",
            "usgs": "science",
            "swpc": "science",
            "bls": "economics",
            "fred": "rates",
        }.get(provider, "unknown")
        return str(event.get("slug", f"market-{m}")), str(event.get("category", category))

    event, category = group(market)
    return (
        amounts.get(market, 0),
        sum(n for m, n in amounts.items() if group(m)[0] == event),
        sum(n for m, n in amounts.items() if group(m)[1] == category),
        sum(amounts.values()),
    )


def allowed_quantity(
    portfolio: dict[str, Any],
    rules: dict[int, dict[str, Any]],
    limits: Limits,
    market: int,
    side: str,
    requested: int,
) -> int:
    """Largest safe quantity. Over-budget portfolios may still reduce their worst exposure."""
    if side not in {"buy", "sell"} or requested < 0:
        raise ValueError("invalid side or quantity")
    before = exposure(portfolio)
    current = totals(before, rules, market)
    caps = (limits.market, limits.event, limits.category, limits.total)
    bounds = tuple(max(cap, used) for cap, used in zip(caps, current, strict=True))

    def fits(qty: int) -> bool:
        proposed = {
            **portfolio,
            "open_orders": [
                *portfolio["open_orders"],
                {"market": market, "side": side, "remaining": qty},
            ],
        }
        after = totals(exposure(proposed), rules, market)
        return all(n <= bound for n, bound in zip(after, bounds, strict=True))

    low, high = 0, requested
    while low < high:
        middle = (low + high + 1) // 2
        if fits(middle):
            low = middle
        else:
            high = middle - 1
    return low
