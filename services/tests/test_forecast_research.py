"""No future training labels, coherent probabilities and explicit missing-data behavior."""

from __future__ import annotations

import copy

import pytest

from brier_services.research.forecasts import evaluate, example_dataset, instant, validate


def test_weather_candidates_use_only_available_training_outcomes() -> None:
    dataset = example_dataset(50)
    result = evaluate(dataset, "2026-01-20", minimum=10)
    assert set(result["scores"]) == {
        "historical",
        "uncalibrated",
        "bias_corrected",
        "conditional",
        "ensemble",
    }
    for row in result["predictions"]:
        assert sum(row["probabilities"]) == pytest.approx(1)
        if "latest_training_resolution" in row["fit"]:
            assert instant(row["fit"]["latest_training_resolution"]) < instant(row["issued_at"])
    assert result["kind"] == "synthetic"
    assert result["paired_vs_historical"]["ensemble"]["paired_forecasts"] > 0


def test_future_labels_cannot_change_earlier_predictions() -> None:
    dataset = example_dataset(40)
    changed = copy.deepcopy(dataset)
    for row in changed["records"]:
        if row["target_date"] >= "2026-02-01":
            row["actual_high"] = 100
    a = evaluate(dataset, "2026-01-20", minimum=10)
    b = evaluate(changed, "2026-01-20", minimum=10)
    assert [r for r in a["predictions"] if r["target_date"] < "2026-02-01"] == [
        r for r in b["predictions"] if r["target_date"] < "2026-02-01"
    ]


def test_late_inputs_and_gapped_outcomes_are_rejected() -> None:
    dataset = example_dataset(2)
    dataset["records"][0]["available_at"] = "2026-02-01T00:00:00Z"
    with pytest.raises(ValueError, match="future"):
        validate(dataset)
    dataset = example_dataset(2)
    dataset["records"][0]["ranges"][1][0] = 48
    with pytest.raises(ValueError, match="gaps"):
        validate(dataset)


def test_missing_ensemble_is_reported_without_fabricated_predictions() -> None:
    dataset = example_dataset(30)
    for row in dataset["records"]:
        row.pop("ensemble")
    result = evaluate(dataset, "2026-01-20", minimum=10)
    assert "ensemble" not in result["scores"]
    assert result["skipped"]["ensemble: insufficient archived ensemble forecasts"] > 0
