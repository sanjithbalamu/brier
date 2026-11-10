"""The forecast reader (recorded NWS responses) and the lister against a fake gateway."""

from __future__ import annotations

import datetime as dt
from pathlib import Path
from typing import Any

from brier_services.catalog import templates
from brier_services.catalog.forecast import Forecasts, forecast_high
from brier_services.catalog.lister import run_once
from brier_services.client import ApiError
from brier_services.resolution.fetch import FetchError, Request, Response

FIXTURES = Path(__file__).parent / "fixtures" / "catalog"
POINTS = (FIXTURES / "nws_points_nyc.json").read_bytes()
FORECAST = (FIXTURES / "nws_forecast_nyc.json").read_bytes()


def test_the_forecast_high_is_the_daytime_period_of_that_local_date() -> None:
    assert forecast_high(FORECAST, dt.date(2026, 11, 8)) == 60
    assert forecast_high(FORECAST, dt.date(2026, 11, 9)) == 58
    assert forecast_high(FORECAST, dt.date(2026, 12, 25)) is None


def test_the_forecast_url_is_looked_up_once_per_city() -> None:
    asked: list[str] = []

    def fetch(request: Request) -> Response:
        asked.append(request.url)
        body = POINTS if "/points/" in request.url else FORECAST
        return Response(request.url, 200, body, "2026-11-08T07:00:00Z")

    f = Forecasts(fetch)
    nyc = templates.CITIES[0]
    assert f.high(nyc, dt.date(2026, 11, 9)) == 58
    assert f.high(nyc, dt.date(2026, 11, 8)) == 60
    assert sum("/points/" in u for u in asked) == 1


class FakeGateway:
    def __init__(self, existing: set[str], refuse: frozenset[int] = frozenset()) -> None:
        self.existing = set(existing)
        self.refuse = refuse
        self.listed: list[dict[str, Any]] = []

    def request(self, method: str, path: str, body: dict[str, Any] | None = None) -> Any:  # noqa: ANN401 - JSON
        if method == "GET":
            if path.endswith("/rules"):
                market = int(path.split("/")[-2])
                return {"rulebook": next(r for r in self.listed if r["market"] == market)}
            slug = path.removeprefix("/v1/events/")
            if slug in self.existing:
                rules = [r for r in self.listed if r["event"]["slug"] == slug]
                if not rules and slug.startswith("nyc-high-"):
                    rules = templates.temperature(
                        templates.CITIES[0], dt.date.fromisoformat(slug[-10:]), 60
                    )
                return {"slug": slug, "markets": [{"market": r["market"]} for r in rules]}
            raise ApiError(404, {"error": {"code": "not_found"}})
        assert body is not None
        rb = body["rulebook"]
        if rb["market"] in self.refuse:
            raise ApiError(400, {"error": {"code": "invalid_rulebook", "message": "refused"}})
        self.listed.append(rb)
        self.existing.add(rb["event"]["slug"])
        return {"market": rb["market"]}


def test_a_pass_lists_two_days_of_every_series_once() -> None:
    def forecast(city: templates.City, day: dt.date) -> int | None:
        if city.slug == "miami":
            msg = "points lookup failed"
            raise FetchError(msg)
        return 60 + city.code

    logs: list[tuple[str, dict[str, Any]]] = []
    gw = FakeGateway(existing={"nyc-high-2026-11-09"})
    now = dt.datetime(2026, 11, 8, 7, 0, tzinfo=dt.UTC)
    listed = run_once(gw, now, forecast, lambda event, **f: logs.append((event, f)))
    # 2 days x (11 cities + M5 count + M6 + geomagnetic), less New York's existing day.
    assert len(listed) == 2 * 14 - 1
    assert "nyc-high-2026-11-10" in listed
    assert not any("miami" in slug for slug in listed)
    assert sum(e == "forecast_unavailable" for e, _ in logs) == 2
    assert len(gw.listed) == 2 * (11 * 7 + 4 + 1 + 4) - 7
    assert run_once(gw, now, forecast, lambda *a, **k: None) == [], "nothing twice"


def test_a_refused_listing_is_reported_and_the_rest_continue() -> None:
    day = dt.date(2026, 11, 9)
    refused = templates.geomagnetic(day)[0]["market"]
    logs: list[str] = []
    gw = FakeGateway(existing=set(), refuse=frozenset({refused}))
    listed = run_once(
        gw, dt.datetime(2026, 11, 8, tzinfo=dt.UTC), lambda c, d: 70, lambda e, **f: logs.append(e)
    )
    assert "geomagnetic-storm-2026-11-09" not in listed
    assert "geomagnetic-storm-2026-11-10" in listed
    assert logs.count("list_failed") == 1


def test_partial_event_resumes_with_original_boundaries_after_forecast_changes() -> None:
    day = dt.date(2026, 11, 9)
    city = templates.CITIES[0]
    original = templates.temperature(city, day, 60)
    gw = FakeGateway(set(), frozenset({original[1]["market"]}))
    now = dt.datetime(2026, 11, 8, tzinfo=dt.UTC)
    run_once(gw, now, lambda c, d: 60, lambda *a, **k: None)
    assert [r for r in gw.listed if r["event"]["slug"] == original[0]["event"]["slug"]] == original[
        :1
    ]
    gw.refuse = frozenset()
    run_once(gw, now, lambda c, d: 75, lambda *a, **k: None)
    completed = [r for r in gw.listed if r["event"]["slug"] == original[0]["event"]["slug"]]
    assert completed == original
    assert run_once(gw, now, lambda c, d: 80, lambda *a, **k: None) == []
