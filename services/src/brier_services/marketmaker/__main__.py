"""python -m brier_services.marketmaker run --gateway URL --key-id N --key-file PATH"""

from __future__ import annotations

import argparse
import math
import os
import signal
import sys
import time
from pathlib import Path
from types import FrameType

from brier_services.client import ApiError, RestClient, clock_ms
from brier_services.marketmaker.execution import OrderExecutor
from brier_services.marketmaker.quoting import Params
from brier_services.marketmaker.risk import Limits
from brier_services.marketmaker.service import FairValues, MarketMaker
from brier_services.resolve import load_key


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="python -m brier_services.marketmaker")
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run", help="quote every market with a fair-value model")
    run.add_argument("--gateway", required=True)
    run.add_argument("--key-id", type=int, required=True)
    run.add_argument(
        "--key-file",
        type=Path,
        required=True,
        help="made by python -m brier_services.resolve keygen",
    )
    run.add_argument("--interval", type=float, default=5.0, help="seconds between passes")
    run.add_argument("--metrics", type=Path, default=Path("var/marketmaker/metrics.jsonl"))
    run.add_argument("--state", type=Path, default=Path("var/marketmaker/orders.sqlite"))
    run.add_argument("--lease-seconds", type=int, default=60)
    run.add_argument("--size", type=int, default=25, help="maximum contracts per quote")
    run.add_argument("--market-limit", type=int, default=500)
    run.add_argument("--event-limit", type=int, default=1000)
    run.add_argument("--category-limit", type=int, default=2500)
    run.add_argument("--total-limit", type=int, default=5000)
    args = parser.parse_args(argv)
    if not math.isfinite(args.interval) or not 0 < args.interval < args.lease_seconds / 2:
        parser.error("interval must be positive and less than half the lease duration")
    if not 5 <= args.lease_seconds <= 300:
        parser.error("lease-seconds must be 5..300")
    if min(args.event_limit, args.category_limit, args.total_limit) < 1:
        parser.error("risk limits must be positive contract-equivalents")
    try:
        params = Params(size=args.size, limit=args.market_limit)
    except ValueError as e:
        parser.error(str(e))
    key = load_key(args.key_file)
    client = RestClient(args.gateway)
    client.use_key(args.key_id, key.sign, clock_ms)
    args.metrics.parent.mkdir(parents=True, exist_ok=True)
    user = client.request("GET", "/v1/me")
    execution = OrderExecutor(client, args.state, f"{client.base_url}:{user['user_id']}")
    maker = MarketMaker(
        client,
        FairValues(os.environ.get("FRED_API_KEY")),
        metrics=args.metrics,
        execution=execution,
        lease_seconds=args.lease_seconds,
        params=params,
        limits=Limits(
            market=args.market_limit,
            event=args.event_limit,
            category=args.category_limit,
            total=args.total_limit,
        ),
    )

    def stop(signum: int, frame: FrameType | None) -> None:
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, stop)
    try:
        maker.heartbeat()
        execution.resolve()
        maker.cancel_all()  # a dedicated maker account never inherits forgotten resting quotes
        while True:
            try:
                maker.run_once()
            except (ApiError, OSError) as e:
                maker.say("pass_failed", error=f"{type(e).__name__}: {e}")
                try:
                    maker.cancel_all()
                except (ApiError, OSError) as cancel_error:
                    maker.say("cancel_failed", error=str(cancel_error))
            time.sleep(args.interval)
    except KeyboardInterrupt:
        maker.say("stopping")
    finally:
        try:
            maker.cancel_all()
        except (ApiError, OSError) as e:
            maker.say("shutdown_cancel_failed", error=str(e))
        execution.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
