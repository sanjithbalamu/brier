"""Exogenous market tapes, generated before a strategy is allowed to act."""

from __future__ import annotations

import math
import random
from dataclasses import asdict, dataclass
from typing import Any

from brier_services.sim.market import logistic


@dataclass(frozen=True)
class Scenario:
    """A fully specified synthetic environment; time is measured in steps."""

    name: str
    steps: int = 480
    signal_sd: float | None = None
    noise_rate: float = 0.5
    information_lag: int = 30
    order_latency: int = 0
    shock_step: int = -1
    shock_logit: float = 0.0
    outage_start: int = -1
    outage_end: int = -1
    feed_freeze_start: int = -1
    feed_freeze_end: int = -1

    def __post_init__(self) -> None:
        if not self.name or not 30 <= self.steps <= 100_000:
            raise ValueError("scenario needs a name and 30..100000 steps")
        if not math.isfinite(self.noise_rate) or not 0 <= self.noise_rate <= 1:
            raise ValueError("noise_rate must be in [0, 1]")
        if self.signal_sd is not None and (not math.isfinite(self.signal_sd) or self.signal_sd < 0):
            raise ValueError("signal_sd must be finite and non-negative")
        if min(self.information_lag, self.order_latency) < 0:
            raise ValueError("delays must be non-negative")
        if not math.isfinite(self.shock_logit):
            raise ValueError("shock must be finite")
        if self.shock_step != -1 and not 0 <= self.shock_step < self.steps:
            raise ValueError("shock step must be within the session or -1 to disable")
        for start, end in (
            (self.outage_start, self.outage_end),
            (self.feed_freeze_start, self.feed_freeze_end),
        ):
            if (start, end) != (-1, -1) and not 0 <= start < end <= self.steps:
                raise ValueError("outage/freeze bounds must be ordered within the session")


SCENARIOS = {
    s.name: s
    for s in (
        Scenario("noise"),
        Scenario("informed", signal_sd=0.25),
        Scenario("shock", signal_sd=0.25, shock_step=240, shock_logit=2.5),
        Scenario("thin", signal_sd=0.5, noise_rate=0.05),
        Scenario("latency", signal_sd=0.25, order_latency=5),
        Scenario("stale_feed", signal_sd=0.25, feed_freeze_start=180, feed_freeze_end=260),
        Scenario("disconnect", signal_sd=0.25, outage_start=180, outage_end=260),
    )
}


def make_tape(seed: int, scenario: Scenario) -> dict[str, Any]:
    """Generate all information, arrivals and the outcome independently of any strategy."""
    truth = random.Random(f"truth-{seed}")  # noqa: S311 - reproducible research
    noise = random.Random(f"noise-{seed}")  # noqa: S311
    informed = random.Random(f"informed-{seed}")  # noqa: S311
    logit = truth.gauss(0, 1)
    path: list[float] = []
    frames: list[dict[str, Any]] = []
    observed = 0
    for step in range(scenario.steps):
        if step == scenario.shock_step:
            logit += scenario.shock_logit
        path.append(logistic(logit))
        if not scenario.feed_freeze_start <= step < scenario.feed_freeze_end:
            observed = step
        frame: dict[str, Any] = {
            "step": step,
            "truth": path[-1],
            "fair": path[max(0, observed - scenario.information_lag)],
            "feed_age": step - observed,
            "connected": not scenario.outage_start <= step < scenario.outage_end,
            "noise": None,
            "informed": None,
        }
        if noise.random() < scenario.noise_rate:
            frame["noise"] = {
                "side": "buy" if noise.random() < 0.5 else "sell",
                "qty": noise.randint(1, 20),
            }
        if scenario.signal_sd is not None and informed.random() < 0.15:
            frame["informed"] = {
                "estimate": logistic(logit + informed.gauss(0, scenario.signal_sd)),
                "qty": informed.randint(1, 20),
            }
        frames.append(frame)
        logit += truth.gauss(0, 0.08)
    return {
        "seed": seed,
        "scenario": asdict(scenario),
        "frames": frames,
        "outcome": "yes" if truth.random() < logistic(logit) else "no",
    }
