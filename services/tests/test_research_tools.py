"""Configuration, public recordings and the local monitor's read-only boundary."""

from __future__ import annotations

import asyncio
import http.client
import json
import threading
from http.server import ThreadingHTTPServer
from pathlib import Path

import pytest
from websockets.asyncio.server import ServerConnection, serve

from brier_services.research.config import configuration
from brier_services.research.experiments import comparisons, verify
from brier_services.research.forecasts import evaluate, example_dataset, save_evaluation
from brier_services.research.history import capture, read_capture, render_history
from brier_services.research.monitor import handler, latest


def test_configuration_is_strict_and_supports_parameter_overrides() -> None:
    data = {
        "schema": 1,
        "seeds": [1001, 1002],
        "scenarios": [{"base": "latency", "name": "slow", "order_latency": 20}],
        "strategies": [{"name": "small", "params": {"size": 3}, "requote_every": 5}],
    }
    scenarios, strategies, seeds = configuration(data)
    assert scenarios[0].order_latency == 20
    assert strategies[0].params.size == 3
    assert seeds == [1001, 1002]
    data["strategies"] = [{"name": "small", "params": {"size": True}}]
    with pytest.raises(ValueError, match="type"):
        configuration(data)
    data["strategies"] = [{"name": "small", "params": {"typo": 3}}]
    with pytest.raises(ValueError, match="unknown"):
        configuration(data)


def test_paired_comparison_rejects_missing_baseline_seeds() -> None:
    rows = [
        {"scenario": "x", "strategy": strategy, "seed": seed, "tape_sha256": "same", "metrics": {}}
        for strategy, seed in (("baseline", 1), ("baseline", 2), ("candidate", 1))
    ]
    with pytest.raises(ValueError, match="seeds"):
        comparisons(rows, "baseline")


def test_forecast_artifacts_are_immutable_and_verified(tmp_path: Path) -> None:
    dataset = example_dataset(30)
    result = evaluate(dataset, "2026-01-20", minimum=10)
    root = save_evaluation(dataset, result, tmp_path)
    assert save_evaluation(dataset, result, tmp_path) == root
    verify(root)
    (root / "report.html").write_text("changed")
    with pytest.raises(ValueError, match="integrity"):
        verify(root)


def test_monitor_tolerates_partial_appends_and_restricts_requests(tmp_path: Path) -> None:
    metrics = tmp_path / "metrics.jsonl"
    metrics.write_text('{"market":1,"t":1,"fair":0.5}\n{"market":2,"t":NaN}\n{"market":')
    assert len(latest(metrics)["markets"]) == 1
    with ThreadingHTTPServer(("127.0.0.1", 0), handler(metrics, 0)) as server:
        port = server.server_port
        server.RequestHandlerClass = handler(metrics, port)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port)
            conn.request("GET", "/api/metrics")
            response = conn.getresponse()
            assert response.status == 200
            assert json.loads(response.read())["markets"][0]["fair"] == 0.5
            conn.request("GET", "/api/metrics", headers={"Host": "untrusted.example"})
            response = conn.getresponse()
            assert response.status == 403
            response.read()
            conn.request("POST", "/v1/orders", body=b"{}")
            response = conn.getresponse()
            assert response.status == 501
            response.read()
            conn.close()
        finally:
            server.shutdown()
            thread.join()


def test_public_recording_replays_gaps_without_stale_books(tmp_path: Path) -> None:
    output = tmp_path / "recording.jsonl"

    async def exchange(ws: ServerConnection) -> None:
        for _ in range(2):
            request = json.loads(await ws.recv())
            assert request["op"] == "subscribe"
        base = {"channel": "book:1", "journal_seq": 1, "state": "open"}
        for message in (
            {**base, "type": "snapshot", "seq": 10, "bids": [[40, 5]], "asks": [[60, 5]]},
            {**base, "type": "delta", "seq": 12, "bids": [[50, 5]], "asks": []},
            {**base, "type": "snapshot", "seq": 12, "bids": [[50, 5]], "asks": [[70, 5]]},
        ):
            await ws.send(json.dumps(message))
        await ws.wait_closed()

    async def run() -> None:
        async with serve(exchange, "127.0.0.1", 0) as server:
            port = server.sockets[0].getsockname()[1]
            await capture(f"ws://127.0.0.1:{port}", 1, 1, output)

    asyncio.run(run())
    recorded = read_capture(output)
    assert recorded["frames"][0]["bids"] == [(40, 5)]
    assert recorded["frames"][1]["bids"] == []
    assert recorded["frames"][1]["synced"] is False
    assert recorded["frames"][2]["bids"] == [(50, 5)]
    assert recorded["frames"][2]["gaps"] == 1
    render_history(output, tmp_path / "history.html")
    assert "__HISTORY_DATA__" not in (tmp_path / "history.html").read_text()
