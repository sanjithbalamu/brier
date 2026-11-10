"""python -m brier_services.research: run, verify, compare, report and replay."""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
from pathlib import Path
from typing import Any

from brier_services.research.config import configuration
from brier_services.research.experiments import comparisons, read_json, run_experiment, verify
from brier_services.research.forecasts import evaluate, example_dataset, save_evaluation
from brier_services.research.history import capture, render_history
from brier_services.research.monitor import serve
from brier_services.research.report import render
from brier_services.research.scenarios import SCENARIOS
from brier_services.research.simulation import STRATEGIES, replay


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="python -m brier_services.research")
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run", help="run paired, reproducible synthetic experiments")
    run.add_argument("--out", type=Path, default=Path("var/research"))
    run.add_argument("--config", type=Path, help="JSON configuration; replaces seed/preset flags")
    run.add_argument("--scenarios", nargs="+", choices=SCENARIOS, default=list(SCENARIOS))
    run.add_argument("--strategies", nargs="+", choices=STRATEGIES, default=list(STRATEGIES))
    run.add_argument("--seeds", type=int, default=10, help="number of consecutive seeds")
    run.add_argument("--start-seed", type=int, default=1)
    for name in ("verify", "replay", "report"):
        command = sub.add_parser(name)
        command.add_argument("directory", type=Path)
        if name == "report":
            command.add_argument("--out", type=Path, required=True)
    compare = sub.add_parser("compare", help="compare two runs with identical input tapes")
    compare.add_argument("baseline", type=Path)
    compare.add_argument("candidate", type=Path)
    compare.add_argument("--strategy", default="reference")
    example = sub.add_parser("weather-example", help="write a labelled synthetic forecast dataset")
    example.add_argument("--out", type=Path, required=True)
    forecast = sub.add_parser(
        "evaluate-weather", help="evaluate an archived forecast dataset chronologically"
    )
    forecast.add_argument("dataset", type=Path)
    forecast.add_argument(
        "--test-start", required=True, help="first prediction date in the test period"
    )
    forecast.add_argument("--min-train", type=int, default=20)
    forecast.add_argument("--window-days", type=int, default=365)
    forecast.add_argument("--out", type=Path, default=Path("var/forecast-research"))
    monitor = sub.add_parser("monitor", help="read-only live bot decisions on localhost")
    monitor.add_argument("--metrics", type=Path, default=Path("var/marketmaker/metrics.jsonl"))
    monitor.add_argument("--port", type=int, default=3010)
    record = sub.add_parser("record-market", help="record anonymous public book and trade messages")
    record.add_argument("--ws", required=True)
    record.add_argument("--market", type=int, required=True)
    record.add_argument("--seconds", type=int, default=300)
    record.add_argument("--out", type=Path, required=True)
    history = sub.add_parser(
        "history-report", help="replay a recorded public market stream offline"
    )
    history.add_argument("recording", type=Path)
    history.add_argument("--out", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        if args.command == "record-market":
            asyncio.run(capture(args.ws, args.market, args.seconds, args.out))
            print(args.out)
        elif args.command == "history-report":
            render_history(args.recording, args.out)
            print(args.out)
        elif args.command == "monitor":
            serve(args.metrics, args.port)
        elif args.command == "weather-example":
            args.out.parent.mkdir(parents=True, exist_ok=True)
            with args.out.open("x") as f:
                json.dump(example_dataset(), f, indent=2)
            print(args.out)
        elif args.command == "evaluate-weather":
            dataset = read_json(args.dataset)
            result = evaluate(dataset, args.test_start, args.min_train, args.window_days)
            directory = save_evaluation(dataset, result, args.out)
            print(directory / "report.html")
        elif args.command == "run":
            if not 1 <= args.seeds <= 10000 or args.start_seed < 0:
                parser.error("use 1..10000 seeds and a non-negative start seed")
            settings = (
                configuration(read_json(args.config))
                if args.config
                else (
                    [SCENARIOS[n] for n in args.scenarios],
                    [STRATEGIES[n] for n in args.strategies],
                    list(range(args.start_seed, args.start_seed + args.seeds)),
                )
            )
            directory = run_experiment(args.out, *settings)
            render(directory, args.out / f"{directory.name}.html")
            print(directory)
            print(args.out / f"{directory.name}.html")
        elif args.command == "report":
            render(args.directory, args.out)
            print(args.out)
        elif args.command == "compare":
            rows: list[dict[str, Any]] = []
            for name, path in (("baseline", args.baseline), ("candidate", args.candidate)):
                verify(path)
                selected = [
                    r
                    for r in read_json(path / "summary.json")["rows"]
                    if r["strategy"] == args.strategy
                ]
                if not selected:
                    raise ValueError(f"strategy {args.strategy} is absent from {path}")
                rows.extend({**r, "strategy": name} for r in selected)
            print(json.dumps(comparisons(rows, "baseline"), indent=2))
        else:
            manifest = verify(args.directory)
            if args.command == "replay":
                traces = [n for n in manifest["files"] if n.startswith("trace-")]
                for name in traces:
                    replay(read_json(args.directory / name)["records"])
                print(f"Verified {len(traces)} complete command/event replays")
            else:
                print(f"Verified {len(manifest['files'])} artifacts")
    except (ValueError, OSError, KeyError, TypeError) as e:
        print(f"research: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
