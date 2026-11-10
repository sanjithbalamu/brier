"""The lister service: lists the next two days of every recurring series (docs/catalog.md).

    python -m brier_services.catalog run --gateway URL --key-id N --key-file PATH [--every S]

Each pass, for each of the next two UTC days and each series, it asks the gateway whether
the day's event exists; if not, it lists the event's markets one by one. Temperature events
need the NWS forecast high to centre their ranges; a city whose forecast is unavailable is
skipped and tried again on the next pass. It runs as a user with the ``lister`` role, which can
list only markets like these.
"""

from __future__ import annotations

import datetime as dt
import json
from collections.abc import Callable
from typing import Any, Protocol

from brier_services.catalog import templates
from brier_services.catalog.forecast import ForecastHigh
from brier_services.client import ApiError
from brier_services.resolution.fetch import FetchError
from brier_services.resolution.sources import MalformedResponseError

DAYS_AHEAD = (1, 2)


class Api(Protocol):
    def request(self, method: str, path: str, body: dict[str, Any] | None = None) -> Any: ...  # noqa: ANN401 - JSON


def _log(event: str, **fields: Any) -> None:  # noqa: ANN401 - JSON fields
    print(json.dumps({"event": event, **fields}, sort_keys=True), flush=True)


def events_for(
    day: dt.date, forecast: ForecastHigh, say: Callable[..., None]
) -> list[list[dict[str, Any]]]:
    """Every series' event for the day, as lists of rulebooks."""
    out: list[list[dict[str, Any]]] = []
    for city in templates.CITIES:
        try:
            high = forecast(city, day)
        except (FetchError, MalformedResponseError, OSError) as e:
            say("forecast_unavailable", city=city.slug, day=day.isoformat(), error=str(e))
            continue
        if high is None:
            say(
                "forecast_unavailable",
                city=city.slug,
                day=day.isoformat(),
                error="no daytime period",
            )
            continue
        out.append(templates.temperature(city, day, high))
    out += templates.earthquakes(day)
    out.append(templates.geomagnetic(day))
    return out


def _existing(api: Api, slug: str) -> dict[str, Any] | None:
    try:
        result = api.request("GET", f"/v1/events/{slug}")
    except ApiError as e:
        if e.status == 404:
            return None
        raise
    return dict(result)


def missing_rulebooks(api: Api, event: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Resume a partial event, using its existing temperature boundaries as the anchor."""
    existing = _existing(api, event[0]["event"]["slug"])
    if existing is None:
        return event
    present = {m["market"] for m in existing["markets"]}
    expected = {r["market"] for r in event}
    if present == expected:
        return []
    if not present or not present <= expected:
        raise ValueError("existing event does not match the expected outcome set")
    pinned = [api.request("GET", f"/v1/markets/{m}/rules")["rulebook"] for m in sorted(present)]
    anchor = pinned[0]
    if anchor["source"]["provider"] == "nws":
        location = anchor["source"]["series"].removeprefix("CLI/")
        city = next((c for c in templates.CITIES if c.location == location), None)
        if city is None:
            raise ValueError("unknown city in partially listed event")
        candidate = next(r for r in event if r["market"] == anchor["market"])
        shift = int(anchor["condition"]["threshold"]) - int(candidate["condition"]["threshold"])
        high = (
            int(event[0]["condition"]["threshold"])
            + templates.TEMPERATURE_WIDTH * (templates.TEMPERATURE_INNER // 2)
            + 2
            + shift
        )
        event = templates.temperature(city, dt.date.fromisoformat(anchor["observation"]), high)
    by_id = {r["market"]: r for r in event}
    if any(rule != by_id[rule["market"]] for rule in pinned):
        raise ValueError(
            "existing rulebooks differ from the pinned template; manual review required"
        )
    return [r for r in event if r["market"] not in present]


def run_once(
    api: Api, now: dt.datetime, forecast: ForecastHigh, say: Callable[..., None] = _log
) -> list[str]:
    """One pass; returns the slugs of the events it listed."""
    listed: list[str] = []
    today = now.astimezone(dt.UTC).date()
    for ahead in DAYS_AHEAD:
        day = today + dt.timedelta(days=ahead)
        for event in events_for(day, forecast, say):
            slug = event[0]["event"]["slug"]
            try:
                missing = missing_rulebooks(api, event)
                if not missing:
                    continue
                for rulebook in missing:
                    api.request(
                        "POST",
                        "/v1/admin/markets",
                        {
                            "rulebook": rulebook,
                            "reason": f"series listing: {rulebook['event']['series']}",
                        },
                    )
            except (ApiError, ValueError) as e:
                say(
                    "list_failed",
                    slug=slug,
                    status=e.status if isinstance(e, ApiError) else None,
                    error=str(e),
                )
                continue
            listed.append(slug)
            say("listed", slug=slug, markets=len(event))
    return listed
