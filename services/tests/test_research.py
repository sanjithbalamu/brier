"""The workbench's scientific controls, integrity and execution failure scenarios."""

from __future__ import annotations

import copy
from pathlib import Path

import pytest

from brier_services.marketmaker.quoting import Params
from brier_services.research.experiments import comparisons, read_json, run_experiment, verify
from brier_services.research.report import render
from brier_services.research.scenarios import Scenario, make_tape
from brier_services.research.simulation import Strategy, replay, simulate


def test_strategies_share_exogenous_data_and_replay_exactly() -> None:
    tape = make_tape(7, Scenario("test", steps=60, signal_sd=0.25))
    before = copy.deepcopy(tape)
    a = simulate(tape, Strategy())
    b = simulate(tape, Strategy("small", Params(size=2, limit=10)))
    assert tape == before
    assert simulate(tape, Strategy()) == a
    assert a["records"] != b["records"]
    replay(a["records"])
    damaged = copy.deepcopy(a["records"])
    damaged[-1]["events"] = []
    with pytest.raises(ValueError, match="diverged"):
        replay(damaged)


def test_future_information_does_not_change_past_decisions() -> None:
    original = make_tape(9, Scenario("test", steps=60))
    changed = copy.deepcopy(original)
    for frame in changed["frames"][40:]:
        frame["fair"] = 0.99
        frame["truth"] = 0.99
    changed["outcome"] = "no" if original["outcome"] == "yes" else "yes"
    a, b = (simulate(t, Strategy()) for t in (original, changed))
    assert a["history"][:40] == b["history"][:40]
    assert [r for r in a["records"] if r["step"] < 40] == [
        r for r in b["records"] if r["step"] < 40
    ]


@pytest.mark.parametrize("latency", [0, 5, 20])
def test_inventory_bound_holds_under_delayed_quotes(latency: int) -> None:
    for seed in range(4):
        result = simulate(
            make_tape(seed, Scenario("test", steps=80, signal_sd=0.0, order_latency=latency)),
            Strategy("tiny", Params(limit=5, size=25)),
        )
        assert result["metrics"]["max_inventory"] <= 5


def test_disconnect_and_stale_feed_withdraw_at_their_deadlines() -> None:
    disconnected = Scenario("test", steps=80, outage_start=20, outage_end=45)
    result = simulate(make_tape(4, disconnected), Strategy(heartbeat_timeout=5))
    for frame in result["history"][25:45]:
        assert frame["bid"] is None
        assert frame["ask"] is None
    frozen = Scenario("test", steps=80, feed_freeze_start=20, feed_freeze_end=45)
    result = simulate(make_tape(4, frozen), Strategy(stale_after=5))
    assert result["metrics"]["stale_quote_steps"] == 0
    assert result["history"][30]["reason"] == "stale input"


def test_run_is_reusable_and_detects_modified_artifacts(tmp_path: Path) -> None:
    scenario = Scenario("small", steps=40)
    strategies = [Strategy(), Strategy("candidate", Params(size=5))]
    path = run_experiment(tmp_path, [scenario], strategies, [1, 2])
    assert run_experiment(tmp_path, [scenario], strategies, [1, 2]) == path
    manifest = verify(path)
    assert manifest["source"]["sha256"]
    summary = read_json(path / "summary.json")
    assert len(summary["rows"]) == 4
    assert summary["comparisons"][0]["candidate_minus_baseline"]["pnl_ticks"]["n"] == 2
    output = tmp_path / "report.html"
    render(path, output)
    assert "__RESEARCH_DATA__" not in output.read_text()
    assert "connect-src 'none'" in output.read_text()
    (path / "summary.json").write_text("{}")
    with pytest.raises(ValueError, match="integrity"):
        verify(path)


def test_comparisons_refuse_different_inputs() -> None:
    rows = [
        {"scenario": "noise", "seed": 1, "strategy": "a", "tape_sha256": "x", "metrics": {}},
        {"scenario": "noise", "seed": 1, "strategy": "b", "tape_sha256": "y", "metrics": {}},
    ]
    with pytest.raises(ValueError, match="identical"):
        comparisons(rows, "a")
