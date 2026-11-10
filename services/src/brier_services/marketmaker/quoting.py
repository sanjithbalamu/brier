"""Quotes after Avellaneda and Stoikov (2008), in probability space, with the parameters
registered in docs/notes/market-maker.md.

- Reservation price: r = p - skew * q_hat * (sigma^2 / 0.25) * tau, where q_hat is
  inventory / limit in [-1, 1], sigma^2 = p(1 - p) (a binary outcome's variance), and tau is
  the fraction of time left.
  Long YES lowers the price the maker wants to pay and sell at, so its inventory mean-reverts.
- Half-spread: max(floor, base + vol * (sigma^2 / 0.25) * tau): wider when the outcome is
  uncertain and far away, never below the floor.
- Quotes in whole ticks, rounded outward, inside 1-99, bid below ask. At the position limit only
  the side that reduces the position is quoted.
"""

from __future__ import annotations

import math
from dataclasses import dataclass


@dataclass(frozen=True)
class Params:
    """Registered values (docs/notes/market-maker.md §3)."""

    size: int = 25
    limit: int = 500
    skew: float = 0.08
    floor: float = 0.02
    base: float = 0.015
    vol: float = 0.02

    def __post_init__(self) -> None:
        if not 1 <= self.size <= 25_000 or self.limit < 1:
            raise ValueError("size must be 1..25000 and inventory limit positive")
        if any(not math.isfinite(x) or x < 0 for x in (self.skew, self.floor, self.base, self.vol)):
            raise ValueError("quoting parameters must be finite and non-negative")


@dataclass(frozen=True)
class Quote:
    """Prices in ticks (1-99); ``None`` means that side is not quoted."""

    bid: int | None
    ask: int | None
    size: int


def quote(p: float, inventory: int, tau: float, params: Params = Params()) -> Quote:  # noqa: B008 - frozen default
    """Where to quote YES, given fair probability ``p``, net YES ``inventory``, and the fraction
    of the market's life left ``tau``."""
    if not 0.0 < p < 1.0 or not math.isfinite(p):
        msg = f"fair probability must be in (0, 1), got {p}"
        raise ValueError(msg)
    tau = min(1.0, max(0.0, tau))
    variance = p * (1.0 - p) / 0.25
    q = max(-1.0, min(1.0, inventory / params.limit))
    reservation = p - params.skew * q * variance * tau
    half = max(params.floor, params.base + params.vol * variance * tau)
    bid = min(98, max(1, math.floor(100 * (reservation - half) + 1e-9)))
    ask = max(2, min(99, math.ceil(100 * (reservation + half) - 1e-9)))
    if bid >= ask:  # only when clamped at an edge: keep one tick apart
        bid, ask = (ask - 1, ask) if ask > 1 else (1, 2)
    # At the limit, quote only the side that brings the position back. One fill can overshoot the
    # limit by at most one quote size (M3 allows exactly that).
    return Quote(
        bid if inventory < params.limit else None,
        ask if inventory > -params.limit else None,
        params.size,
    )
