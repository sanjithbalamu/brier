"""Portable, offline research dashboard. No server, external scripts or trading controls."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from brier_services.research.experiments import read_json, verify


def render(directory: Path, output: Path) -> None:
    """Embed verified results and diagnostic replay frames in one standalone HTML file."""
    manifest = verify(directory)
    summary = read_json(directory / "summary.json")
    traces = []
    for row in summary["rows"]:
        trace = read_json(directory / row["trace"])
        trades = [
            {"step": r["step"], **e}
            for r in trace["records"]
            for e in r["events"]
            if e["ev"] == "trade"
        ]
        traces.append(
            {**row, "history": trace["history"], "trades": trades, "outcome": trace["outcome"]}
        )
    payload = {"manifest": manifest, "summary": summary, "traces": traces}
    write_page(payload, output)


def write_page(payload: dict[str, Any], output: Path) -> None:
    """Escape script terminators even when an artifact contains hostile strings."""
    template = Path(__file__).with_name("dashboard.html").read_text()
    data = json.dumps(payload, allow_nan=False).replace("<", "\\u003c").replace("&", "\\u0026")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(template.replace("__RESEARCH_DATA__", data))
