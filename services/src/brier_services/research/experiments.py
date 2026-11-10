"""Content-addressed experiment artifacts and paired, seed-matched comparisons."""

from __future__ import annotations

import gzip
import hashlib
import json
import platform
import random
import shutil
import statistics
import subprocess
import tempfile
from dataclasses import asdict
from pathlib import Path
from typing import Any

from brier_services.research.scenarios import Scenario, make_tape
from brier_services.research.simulation import Strategy, replay, simulate

SCHEMA = 1


def canonical(value: Any) -> bytes:  # noqa: ANN401 - artifact JSON
    """Portable JSON, rejecting non-finite numbers instead of writing invalid JSON."""
    return (
        json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False) + "\n"
    ).encode()


def digest(data: bytes) -> str:
    """SHA-256 of exact stored bytes."""
    return hashlib.sha256(data).hexdigest()


def read_json(path: Path) -> Any:  # noqa: ANN401 - validated at the consumer
    """Read ordinary JSON or a gzip-compressed experiment trace."""
    data = path.read_bytes()
    return json.loads(gzip.decompress(data) if path.suffix == ".gz" else data)


def source_snapshot() -> tuple[dict[str, str], dict[str, Any]]:
    """Archive the Python sources, package metadata and lock without local secrets."""
    package = Path(__file__).resolve().parents[1]
    services = package.parent.parent
    files = {
        str(p.relative_to(services)): p.read_text()
        for p in sorted(package.rglob("*"))
        if p.is_file() and p.suffix in {".py", ".html"}
    }
    for name in ("pyproject.toml", "uv.lock"):
        p = services / name
        if p.exists():
            files[name] = p.read_text()
    commit = "unavailable"
    dirty = True
    git = shutil.which("git")
    try:
        if git is None:
            raise FileNotFoundError("git is not installed")
        commit = (
            subprocess.check_output(  # noqa: S603 - fixed git read, no shell
                [git, "-C", str(services), "rev-parse", "HEAD"], stderr=subprocess.DEVNULL
            )
            .decode()
            .strip()
        )
        dirty = bool(
            subprocess.check_output(  # noqa: S603
                [git, "-C", str(services), "status", "--porcelain"], stderr=subprocess.DEVNULL
            ).strip()
        )
    except (OSError, subprocess.CalledProcessError):
        pass
    return files, {
        "commit": commit,
        "dirty": dirty,
        "sha256": digest(canonical(files)),
        "python": platform.python_version(),
    }


def interval(values: list[float]) -> dict[str, float | int | None]:
    """Seed bootstrap of the mean; one sample cannot support an uncertainty interval."""
    if not values:
        raise ValueError("cannot summarize an empty sample")
    mean = statistics.fmean(values)
    if len(values) < 2:
        return {"n": len(values), "mean": mean, "low": None, "high": None}
    rng = random.Random(20261109)  # noqa: S311 - fixed bootstrap resampling
    means = sorted(statistics.fmean(rng.choices(values, k=len(values))) for _ in range(1000))
    return {"n": len(values), "mean": mean, "low": means[24], "high": means[974]}


def comparisons(rows: list[dict[str, Any]], baseline: str) -> list[dict[str, Any]]:
    """Compare only exact common tapes; do not pool unrelated scenarios or missing seeds."""
    index = {(r["scenario"], r["seed"], r["strategy"]): r for r in rows}
    if len(index) != len(rows):
        raise ValueError("duplicate scenario/seed/strategy result")
    baseline_keys = {(r["scenario"], r["seed"]) for r in rows if r["strategy"] == baseline}
    for strategy in {r["strategy"] for r in rows} - {baseline}:
        keys = {(r["scenario"], r["seed"]) for r in rows if r["strategy"] == strategy}
        if keys != baseline_keys:
            raise ValueError("paired comparison requires identical scenarios and seeds")
    out = []
    for scenario in sorted({r["scenario"] for r in rows}):
        for strategy in sorted({r["strategy"] for r in rows} - {baseline}):
            differences: dict[str, list[float]] = {}
            for row in rows:
                if row["strategy"] != strategy or row["scenario"] != scenario:
                    continue
                other = index.get((scenario, row["seed"], baseline))
                if other is None or other["tape_sha256"] != row["tape_sha256"]:
                    raise ValueError(
                        "paired comparison requires identical scenarios and input tapes"
                    )
                for metric, value in row["metrics"].items():
                    if value is not None and other["metrics"].get(metric) is not None:
                        differences.setdefault(metric, []).append(
                            float(value - other["metrics"][metric])
                        )
            if differences:
                out.append(
                    {
                        "scenario": scenario,
                        "candidate": strategy,
                        "baseline": baseline,
                        "candidate_minus_baseline": {
                            k: interval(v) for k, v in differences.items()
                        },
                    }
                )
    return out


def verify(directory: Path) -> dict[str, Any]:
    """Verify integrity before consuming artifacts. Hashes detect changes, not authorship."""
    manifest = read_json(directory / "manifest.json")
    if manifest.get("schema") != SCHEMA:
        raise ValueError("unsupported experiment schema")
    base = directory.resolve()
    for name, expected in manifest["files"].items():
        path = (directory / name).resolve()
        if not path.is_relative_to(base) or digest(path.read_bytes()) != expected:
            raise ValueError(f"artifact failed integrity check: {name}")
    return dict(manifest)


def run_experiment(
    output: Path, scenarios: list[Scenario], strategies: list[Strategy], seeds: list[int]
) -> Path:
    """Write a complete immutable run, publishing its directory only after success."""
    if not seeds or len(seeds) != len(set(seeds)) or any(s < 0 for s in seeds):
        raise ValueError("provide distinct non-negative seeds")
    for group in (scenarios, strategies):
        names = [x.name for x in group]
        if not names or len(names) != len(set(names)):
            raise ValueError("provide distinct scenario and strategy names")
        if any(
            not n or any(c not in "abcdefghijklmnopqrstuvwxyz0123456789_-" for c in n)
            for n in names
        ):
            raise ValueError("names must contain lowercase letters, numbers, _ or -")
    sources, provenance = source_snapshot()
    config = {
        "schema": SCHEMA,
        "source": provenance,
        "scenarios": [asdict(s) for s in scenarios],
        "strategies": [asdict(s) for s in strategies],
        "seeds": seeds,
    }
    run_id = digest(canonical(config))[:20]
    output.mkdir(parents=True, exist_ok=True)
    destination = output / run_id
    if destination.exists():
        verify(destination)
        return destination
    with tempfile.TemporaryDirectory(prefix=".running-", dir=output) as scratch:
        root = Path(scratch)
        files: dict[str, str] = {}

        def write(name: str, value: Any) -> None:  # noqa: ANN401 - artifact JSON
            data = canonical(value)
            if name.endswith(".gz"):
                data = gzip.compress(data, mtime=0)
            (root / name).write_bytes(data)
            files[name] = digest(data)

        write("source.json.gz", sources)
        rows = []
        for scenario in scenarios:
            for seed in seeds:
                tape = make_tape(seed, scenario)
                tape_hash = digest(canonical(tape))
                write(f"tape-{scenario.name}-{seed}.json.gz", tape)
                for strategy in strategies:
                    result = simulate(tape, strategy)
                    replay(result["records"])
                    name = f"trace-{scenario.name}-{seed}-{strategy.name}.json.gz"
                    write(name, result)
                    rows.append(
                        {
                            "scenario": scenario.name,
                            "seed": seed,
                            "strategy": strategy.name,
                            "tape_sha256": tape_hash,
                            "trace": name,
                            "metrics": result["metrics"],
                        }
                    )
        summary = {
            "run_id": run_id,
            "kind": "synthetic simulation",
            "rows": rows,
            "comparisons": comparisons(rows, strategies[0].name),
        }
        write("summary.json", summary)
        manifest = {**config, "run_id": run_id, "files": files}
        (root / "manifest.json").write_bytes(canonical(manifest))
        # rename inside one filesystem; existing results are never overwritten.
        root.rename(destination)
    return destination
