"""Strategy execution on a fixed tape, with latency and protective cancellation."""

from __future__ import annotations

from dataclasses import asdict, dataclass, field
from typing import Any

from brier_services.marketmaker.quoting import Params, quote
from brier_services.reference_engine import ReferenceEngine
from brier_services.sim.market import INFORMED, MAKER, MARKET, NOISE, Exchange


@dataclass(frozen=True)
class Strategy:
    """A strategy version including its execution controls."""

    name: str = "reference"
    params: Params = field(default_factory=Params)
    stale_after: int = 10
    heartbeat_timeout: int = 10
    requote_every: int = 1

    def __post_init__(self) -> None:
        if not self.name or min(self.stale_after, self.heartbeat_timeout, self.requote_every) < 1:
            raise ValueError("strategy needs a name and positive safety intervals")


STRATEGIES = {
    "reference": Strategy(),
    "conservative": Strategy("conservative", Params(size=10, limit=150, floor=0.03, skew=0.12)),
}


class RecordedExchange(Exchange):
    """Record the exact commands and engine replies, including rejected commands."""

    def __init__(self) -> None:
        super().__init__()
        self.step = -1
        self.records: list[dict[str, Any]] = []

    def send(self, **cmd: Any) -> list[dict[str, Any]]:  # noqa: ANN401 - wire data
        events = super().send(**cmd)
        self.records.append(
            {"step": self.step, "command": {"seq": self.seq, **cmd}, "events": events}
        )
        return events


def replay(records: list[dict[str, Any]]) -> ReferenceEngine:
    """Verify every recorded reply; refuse a corrupted or incompatible tape."""
    engine = ReferenceEngine()
    for record in records:
        actual = engine.apply(record["command"])
        if actual != record["events"]:
            raise ValueError(f"replay diverged at sequence {record['command']['seq']}")
    return engine


def simulate(tape: dict[str, Any], strategy: Strategy) -> dict[str, Any]:
    """Run without drawing random numbers or consulting future tape entries."""
    ex = RecordedExchange()
    for account in (MAKER, NOISE, INFORMED):
        ex.send(cmd="open_account", account=account)
    ex.send(cmd="create_market", market=MARKET)
    ex.send(cmd="set_state", market=MARKET, state="open")
    start = ex.engine.cash[MAKER]
    steps = len(tape["frames"])
    latency = tape["scenario"]["order_latency"]
    pending: list[tuple[int, dict[str, int | None]]] = []
    history: list[dict[str, Any]] = []
    last_heartbeat = 0
    peak = float(start)
    drawdown = 0.0
    quoted = stale_quoted = spread_sum = 0
    max_inventory = 0
    for frame in tape["frames"]:
        step = int(frame["step"])
        ex.step = step
        if frame["connected"]:
            last_heartbeat = step
        expired = step - last_heartbeat >= strategy.heartbeat_timeout
        stale = frame["feed_age"] >= strategy.stale_after
        closing = step >= steps - 15
        reason = (
            "close buffer"
            if closing
            else "heartbeat expired"
            if expired
            else "stale input"
            if stale
            else None
        )
        # Local feed checks require connectivity; server-side lease expiry does not.
        pull = expired or (frame["connected"] and (stale or closing))
        if pull:
            pending.clear()
            for order in ex.resting(MAKER):
                ex.send(cmd="cancel", account=MAKER, order=order)
        elif frame["connected"] and step % strategy.requote_every == 0 and not pending:
            fair = min(0.99, max(0.01, frame["fair"]))
            q = quote(fair, ex.position(MAKER), (steps - step) / steps, strategy.params)
            pending.append((step + latency, {"buy": q.bid, "sell": q.ask}))
        if pending and pending[0][0] <= step and not pull:
            _, target = pending.pop(0)
            for order in ex.resting(MAKER):
                ex.send(cmd="cancel", account=MAKER, order=order)
            for side, price in target.items():
                if price is not None:
                    # Hard per-market bound, even if the position changed during latency.
                    inventory = ex.position(MAKER)
                    capacity = strategy.params.limit - (inventory if side == "buy" else -inventory)
                    qty = min(strategy.params.size, max(0, capacity))
                    if qty:
                        ex.send(
                            cmd="place",
                            account=MAKER,
                            market=MARKET,
                            side=side,
                            price=price,
                            qty=qty,
                            tif="gtc",
                            post_only=True,
                        )
        bid, ask = ex.best()
        if bid is not None and ask is not None:
            quoted += 1
            spread_sum += ask - bid
        if ex.resting(MAKER) and (stale or expired):
            stale_quoted += 1
        for account, intent in ((NOISE, frame["noise"]), (INFORMED, frame["informed"])):
            if intent is None:
                continue
            bid, ask = ex.best()
            if account == NOISE:
                side = intent["side"]
            elif ask is not None and intent["estimate"] > ask / 100 + 0.02:
                side = "buy"
            elif bid is not None and intent["estimate"] < bid / 100 - 0.02:
                side = "sell"
            else:
                continue
            price = ask if side == "buy" else bid
            if price is not None:
                ex.send(
                    cmd="place",
                    account=account,
                    market=MARKET,
                    side=side,
                    price=price,
                    qty=intent["qty"],
                    tif="ioc",
                    post_only=False,
                )
        inventory = ex.position(MAKER)
        max_inventory = max(max_inventory, abs(inventory))
        equity = (
            ex.engine.cash[MAKER]
            + max(0, inventory) * frame["fair"] * 100
            + max(0, -inventory) * (1 - frame["fair"]) * 100
        )
        peak = max(peak, equity)
        drawdown = max(drawdown, peak - equity)
        history.append(
            {
                "step": step,
                "fair": frame["fair"],
                "truth": frame["truth"],
                "bid": bid,
                "ask": ask,
                "inventory": inventory,
                "equity_ticks": round(equity, 4),
                "feed_age": frame["feed_age"],
                "connected": frame["connected"],
                "reason": reason if pull else None,
            }
        )
    ex.step = steps
    ex.send(cmd="set_state", market=MARKET, state="closed")
    ex.send(cmd="settle", market=MARKET, outcome=tape["outcome"])
    final_cash = ex.engine.cash[MAKER]
    drawdown = max(drawdown, peak - final_cash)
    events = [e for r in ex.records for e in r["events"]]
    trades = [e for e in events if e["ev"] == "trade"]
    return {
        "seed": tape["seed"],
        "scenario": tape["scenario"]["name"],
        "strategy": asdict(strategy),
        "outcome": tape["outcome"],
        "metrics": {
            "pnl_ticks": final_cash - start,
            "max_inventory": max_inventory,
            "max_drawdown_ticks": round(drawdown, 4),
            "trades": len(trades),
            "contracts": sum(t["qty"] for t in trades),
            "quote_share": quoted / steps,
            "stale_quote_steps": stale_quoted,
            "average_spread_ticks": spread_sum / quoted if quoted else None,
            "rejected": sum(e["ev"] == "rejected" for e in events),
        },
        "history": history,
        "records": ex.records,
    }
