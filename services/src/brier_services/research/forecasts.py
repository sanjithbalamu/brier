"""Chronological weather evaluation with availability checks and candidate-only models."""

from __future__ import annotations

import datetime as dt
import html
import math
import random
import statistics
import tempfile
from collections import defaultdict
from pathlib import Path
from typing import Any

from brier_services.catalog.models import brier, empirical_probs, normal_probs, outcome_index
from brier_services.research.experiments import canonical, digest, interval, source_snapshot, verify

MODELS = ("historical", "uncalibrated", "bias_corrected", "conditional", "ensemble")


def instant(value: str) -> dt.datetime:
    """Require an explicit timezone, avoiding silently local historical cutoffs."""
    parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError("forecast timestamps require a timezone")
    return parsed.astimezone(dt.UTC)


def validate(dataset: dict[str, Any]) -> list[dict[str, Any]]:
    """Reject look-ahead inputs, duplicate examples and malformed distributions."""
    if dataset.get("schema") != 1 or dataset.get("kind") not in {"observed", "synthetic"}:
        raise ValueError("dataset requires schema=1 and kind=observed or synthetic")
    if not isinstance(dataset.get("source"), str) or not dataset["source"]:
        raise ValueError("dataset must identify its source")
    records = dataset["records"]
    if not records:
        raise ValueError("empty forecast dataset")
    seen = set()
    for row in records:
        key = row["id"]
        if key in seen:
            raise ValueError(f"duplicate forecast id: {key}")
        seen.add(key)
        issued, available, resolved = (
            instant(row[k]) for k in ("issued_at", "available_at", "resolved_at")
        )
        if available > issued or resolved <= issued:
            raise ValueError(f"future input or already resolved prediction: {key}")
        horizon = row["horizon_hours"]
        if (
            not isinstance(row["city"], str)
            or not row["city"]
            or type(horizon) not in (int, float)
            or not math.isfinite(horizon)
            or not 0 < horizon <= 24 * 16
        ):
            raise ValueError("city and a forecast horizon of 1..384 hours are required")
        dt.date.fromisoformat(row["target_date"])
        values = [row["forecast_high"], row["actual_high"], *row.get("ensemble", [])]
        if any(
            isinstance(v, bool) or not isinstance(v, (float, int)) or not math.isfinite(v)
            for v in values
        ):
            raise ValueError("forecast values must be finite numbers")
        if not isinstance(row["actual_high"], int):
            raise ValueError("actual_high must use the contract's whole-degree reading")
        ranges = row["ranges"]
        if len(ranges) < 2 or ranges[0][0] is not None or ranges[-1][1] is not None:
            raise ValueError("ranges must be exhaustive, including both unbounded tails")
        for i, (low, high) in enumerate(ranges):
            if any(
                v is not None and (isinstance(v, bool) or not isinstance(v, int))
                for v in (low, high)
            ):
                raise ValueError("range boundaries must be integers or null")
            if low is not None and high is not None and low > high:
                raise ValueError("reversed range")
            if i and (low is None or ranges[i - 1][1] is None or low != ranges[i - 1][1] + 1):
                raise ValueError("ranges must not overlap or leave gaps")
    return sorted(records, key=lambda r: (instant(r["issued_at"]), r["id"]))


def season(row: dict[str, Any]) -> int:
    """Quarter of the target month; fixed before evaluation, not selected from results."""
    return (dt.date.fromisoformat(row["target_date"]).month - 1) // 3


def probabilities(
    name: str, row: dict[str, Any], training: list[dict[str, Any]], minimum: int
) -> tuple[list[float], dict[str, Any]]:
    """Candidate distribution and explicit fit provenance, using passed historical rows only."""
    ranges = row["ranges"]
    local = [r for r in training if r["city"] == row["city"]]
    if len(local) < minimum:
        raise ValueError("insufficient historical data for this city")
    selected = local
    group = "city"
    if name == "conditional":
        horizon = [r for r in local if r["horizon_hours"] // 24 == row["horizon_hours"] // 24]
        seasonal = [r for r in horizon if season(r) == season(row)]
        if len(seasonal) >= minimum:
            selected, group = seasonal, "city/horizon/season"
        elif len(horizon) >= minimum:
            selected, group = horizon, "city/horizon"
    if name == "ensemble":
        selected = [r for r in local if r.get("ensemble")]
        if not row.get("ensemble") or len(selected) < minimum:
            raise ValueError("insufficient archived ensemble forecasts")
    provenance: dict[str, Any] = {
        "fit_n": len(selected),
        "group": group,
        "latest_training_resolution": max((r["resolved_at"] for r in selected), key=instant),
    }
    if name == "historical":
        return empirical_probs([r["actual_high"] for r in selected], ranges), provenance
    if name == "uncalibrated":
        return normal_probs(row["forecast_high"], 4.0, ranges), {"fit_n": 0, "group": "fixed sd=4F"}
    errors = [
        r["actual_high"]
        - (statistics.fmean(r["ensemble"]) if name == "ensemble" else r["forecast_high"])
        for r in selected
    ]
    bias = statistics.fmean(errors)
    variance = statistics.variance(errors)
    if name == "ensemble":
        spread = statistics.fmean(statistics.pvariance(r["ensemble"]) for r in selected)
        sd = math.sqrt(max(0.25, variance - spread))
        members = [normal_probs(v + bias, sd, ranges) for v in row["ensemble"]]
        probs = [statistics.fmean(v) for v in zip(*members, strict=True)]
    else:
        sd = math.sqrt(max(0.25, variance))
        probs = normal_probs(row["forecast_high"] + bias, sd, ranges)
    return probs, {**provenance, "bias_f": bias, "residual_sd_f": sd}


def evaluate(
    dataset: dict[str, Any], test_start: str, minimum: int = 20, window_days: int = 365
) -> dict[str, Any]:
    """Prequential evaluation: refits use only outcomes resolved before each prediction.

    Fixed model definitions are never selected/tuned using the reported test period. Paired
    intervals resample target days to keep cities on the same day in the same cluster.
    """
    if minimum < 2 or window_days < 1:
        raise ValueError("minimum must be at least 2 and window-days positive")
    cutoff = dt.date.fromisoformat(test_start)
    records = validate(dataset)
    predictions = []
    skipped: dict[str, int] = defaultdict(int)
    for row in records:
        if instant(row["issued_at"]).date() < cutoff:
            continue
        issued = instant(row["issued_at"])
        earliest = issued - dt.timedelta(days=window_days)
        training = [r for r in records if earliest <= instant(r["resolved_at"]) < issued]
        outcome = outcome_index(row["actual_high"], row["ranges"])
        for name in MODELS:
            try:
                probs, fit = probabilities(name, row, training, minimum)
            except ValueError as e:
                skipped[f"{name}: {e}"] += 1
                continue
            if any(not 0 <= p <= 1 for p in probs) or abs(sum(probs) - 1) > 1e-8:
                raise ValueError("model produced an incoherent probability distribution")
            predictions.append(
                {
                    "id": row["id"],
                    "model": name,
                    "city": row["city"],
                    "target_date": row["target_date"],
                    "issued_at": row["issued_at"],
                    "horizon_hours": row["horizon_hours"],
                    "season": season(row),
                    "probabilities": probs,
                    "outcome": outcome,
                    "brier": brier(probs, outcome),
                    "fit": fit,
                }
            )
    if not predictions:
        raise ValueError("no evaluable forecasts; supply earlier resolved training examples")
    scores = {}
    calibration = {}
    baseline = {r["id"]: r for r in predictions if r["model"] == "historical"}
    pairs = {}
    breakdown = []
    for name in MODELS:
        rows = [r for r in predictions if r["model"] == name]
        if not rows:
            continue
        scores[name] = {"n": len(rows), "brier": statistics.fmean(r["brier"] for r in rows)}
        bins: list[list[tuple[float, int]]] = [[] for _ in range(10)]
        for row in rows:
            for i, p in enumerate(row["probabilities"]):
                bins[min(9, int(p * 10))].append((p, int(i == row["outcome"])))
        calibration[name] = [
            {
                "bin": i,
                "n": len(b),
                "predicted": statistics.fmean(p for p, _ in b),
                "observed": statistics.fmean(y for _, y in b),
            }
            for i, b in enumerate(bins)
            if b
        ]
        days: dict[str, list[float]] = defaultdict(list)
        paired = []
        for row in rows:
            if row["id"] in baseline:
                delta = row["brier"] - baseline[row["id"]]["brier"]
                paired.append(delta)
                days[row["target_date"]].append(delta)
        if paired:
            pairs[name] = {
                "paired_forecasts": len(paired),
                "mean_difference": statistics.fmean(paired),
                "equal_day_weighted": interval([statistics.fmean(v) for v in days.values()]),
            }
        for dimension in ("city", "horizon_hours", "season"):
            for value in sorted({r[dimension] for r in rows}):
                group = [r for r in rows if r[dimension] == value]
                breakdown.append(
                    {
                        "model": name,
                        "dimension": dimension,
                        "value": value,
                        "n": len(group),
                        "brier": statistics.fmean(r["brier"] for r in group),
                    }
                )
    return {
        "schema": 1,
        "kind": dataset["kind"],
        "source": dataset["source"],
        "dataset_sha256": digest(canonical(dataset)),
        "test_start": test_start,
        "minimum_training": minimum,
        "window_days": window_days,
        "evaluation": "chronological expanding/refitting; only already resolved training outcomes",
        "promotion": "research only; no automatic live-model promotion",
        "scores": scores,
        "paired_vs_historical": pairs,
        "calibration": calibration,
        "breakdown": breakdown,
        "skipped": dict(skipped),
        "predictions": predictions,
    }


def example_dataset(days: int = 120) -> dict[str, Any]:
    """An explicitly synthetic integration example; never evidence of predictive skill."""
    rng = random.Random(20261109)  # noqa: S311 - example fixture
    records = []
    for day in range(days):
        date = dt.date(2026, 1, 1) + dt.timedelta(days=day)
        for city in ("nyc", "chicago"):
            forecast = 50 + 10 * math.sin(day / 20) + rng.gauss(0, 4)
            actual = round(forecast + 2 + rng.gauss(0, 3))
            records.append(
                {
                    "id": f"{city}-{date}",
                    "city": city,
                    "issued_at": f"{date - dt.timedelta(days=1)}T12:00:00Z",
                    "available_at": f"{date - dt.timedelta(days=1)}T11:00:00Z",
                    "resolved_at": f"{date + dt.timedelta(days=1)}T12:00:00Z",
                    "target_date": str(date),
                    "horizon_hours": 36,
                    "forecast_high": forecast,
                    "actual_high": actual,
                    "ensemble": [forecast + rng.gauss(0, 2) for _ in range(10)],
                    "ranges": [[None, 45], [46, 50], [51, 55], [56, 60], [61, None]],
                }
            )
    return {
        "schema": 1,
        "kind": "synthetic",
        "source": "seeded workflow demonstration",
        "records": records,
    }


def save_evaluation(dataset: dict[str, Any], result: dict[str, Any], output: Path) -> Path:
    """Save the inputs, source snapshot and full evaluation together without overwriting."""
    sources, source = source_snapshot()
    identity = {
        "dataset_sha256": result["dataset_sha256"],
        "source": source,
        "test_start": result["test_start"],
        "minimum": result["minimum_training"],
        "window_days": result["window_days"],
    }
    root = output / digest(canonical(identity))[:20]
    if root.exists():
        verify(root)
        return root
    rows = "".join(
        f"<tr><td>{html.escape(name)}</td><td>{score['n']}</td><td>{score['brier']:.4f}</td></tr>"
        for name, score in result["scores"].items()
    )
    bins = "".join(
        f"<tr><td>{html.escape(name)}</td><td>{b['n']}</td><td>{b['predicted']:.1%}</td><td>{b['observed']:.1%}</td></tr>"
        for name, values in result["calibration"].items()
        for b in values
    )
    note = (
        "Synthetic workflow demonstration. These scores are not evidence of a live edge."
        if result["kind"] == "synthetic"
        else (
            "Observed archive evaluation. Results depend on the supplied availability "
            "timestamps and source provenance."
        )
    )
    page = f"""<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Brier forecast research</title>
<style>body{{font:16px/1.6 system-ui;
max-width:1000px;
margin:40px auto;
padding:20px;
background:#0c1115;
color:#ebf2f5}}table{{border-collapse:collapse;
width:100%}}td,th{{padding:10px;
text-align:left;
border-bottom:1px solid #30414b}}pre{{white-space:pre-wrap;
overflow-wrap:anywhere}}a{{color:#63e7ac}}</style>
<h1>Forecast research</h1>
<p>{html.escape(note)}</p>
<p>Chronological evaluation from {html.escape(result["test_start"])}.
Lower Brier scores are better. Model selection is fixed before evaluation;
each fit only uses outcomes resolved before that prediction.</p>
<h2>Candidate scores</h2>
<table>
<thead>
<tr>
<th>Model</th>
<th>Forecasts</th>
<th>Brier score</th>
</tr>
</thead>
<tbody>{rows}</tbody>
</table>
<h2>Paired comparison with historical frequencies</h2>
<p>Negative differences favor the candidate. Intervals resample target days;
the interval's mean weights days equally. Compare models on shared examples,
especially when ensemble archives are missing.</p>
<pre>{html.escape(json_dump(result["paired_vs_historical"]))}</pre>
<h2>Calibration</h2>
<p>Descriptive bins pooled across outcomes;
 a bin's count is not a count of independent events.</p>
<table>
<thead>
<tr>
<th>Model</th>
<th>Outcome forecasts</th>
<th>Mean predicted</th>
<th>Observed frequency</th>
</tr>
</thead>
<tbody>{bins}</tbody>
</table>
<details>
<summary>City, horizon and season breakdown</summary>
<pre>{html.escape(json_dump(result["breakdown"]))}</pre>
</details>
<h2>Provenance</h2>
<p>Dataset SHA-256: {result["dataset_sha256"]}</p>
<p>Source snapshot: {source["sha256"]}</p>
<p>No candidate is automatically promoted to live quoting.</p>
<details>
<summary>Skipped forecasts</summary>
<pre>{html.escape(json_dump(result["skipped"]))}</pre>
</details>
</html>"""
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".evaluating-", dir=output) as scratch:
        stage = Path(scratch)
        artifacts = {
            "dataset.json": canonical(dataset),
            "evaluation.json": canonical(result),
            "source.json": canonical(sources),
            "report.html": page.encode(),
        }
        hashes = {}
        for name, data in artifacts.items():
            (stage / name).write_bytes(data)
            hashes[name] = digest(data)
        (stage / "manifest.json").write_bytes(canonical({"schema": 1, **identity, "files": hashes}))
        stage.rename(root)
    return root


def json_dump(value: Any) -> str:  # noqa: ANN401 - report JSON
    return canonical(value).decode()
