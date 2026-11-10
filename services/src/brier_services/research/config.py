"""Strict, data-only experiment configuration; no dynamically imported strategy code."""

from __future__ import annotations

import math
from dataclasses import asdict
from typing import Any

from brier_services.marketmaker.quoting import Params
from brier_services.research.scenarios import SCENARIOS, Scenario
from brier_services.research.simulation import STRATEGIES, Strategy


def override(defaults: dict[str, Any], changes: dict[str, Any]) -> dict[str, Any]:
    """Reject unknown keys and implicit bool/string-to-number conversions."""
    if not isinstance(changes, dict) or changes.keys() - defaults.keys():
        raise ValueError("configuration contains unknown fields")
    for key, value in changes.items():
        default = defaults[key]
        if isinstance(default, int):
            valid = type(value) is int
        elif isinstance(default, float) or default is None:
            valid = (
                value is None
                if default is None and value is None
                else (type(value) in (int, float) and math.isfinite(value))
            )
        else:
            valid = isinstance(value, type(default))
        if not valid:
            raise ValueError(f"invalid value type for {key}")
    return defaults | changes


def configuration(data: dict[str, Any]) -> tuple[list[Scenario], list[Strategy], list[int]]:
    """Load explicit seeds, preset-derived scenarios and strategy parameter overrides."""
    if set(data) != {"schema", "seeds", "scenarios", "strategies"} or data["schema"] != 1:
        raise ValueError("configuration requires schema=1, seeds, scenarios and strategies")
    seeds = data["seeds"]
    if (
        not isinstance(seeds, list)
        or not 1 <= len(seeds) <= 10000
        or any(type(s) is not int or s < 0 for s in seeds)
        or len(set(seeds)) != len(seeds)
    ):
        raise ValueError("provide 1..10000 distinct non-negative integer seeds")
    scenarios = []
    for item in data["scenarios"]:
        item = dict(item)
        base = item.pop("base", "noise")
        if base not in SCENARIOS:
            raise ValueError(f"unknown scenario preset: {base}")
        scenarios.append(Scenario(**override(asdict(SCENARIOS[base]), item)))
    strategies = []
    for item in data["strategies"]:
        item = dict(item)
        base = item.pop("base", "reference")
        if base not in STRATEGIES:
            raise ValueError(f"unknown strategy preset: {base}")
        defaults = asdict(STRATEGIES[base])
        params = Params(**override(defaults.pop("params"), item.pop("params", {})))
        strategies.append(Strategy(**override(defaults, item), params=params))
    return scenarios, strategies, seeds
