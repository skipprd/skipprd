"""Parse skipprd::perf PERF_EVENT lines from captured logs."""

from __future__ import annotations

import json
from typing import Any

PREFIX = "PERF_EVENT "


class PerfParseError(ValueError):
    pass


def parse_perf_events(text: str) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    for line in text.splitlines():
        idx = line.find(PREFIX)
        if idx < 0:
            continue
        payload = line[idx + len(PREFIX) :].strip()
        try:
            parsed = json.loads(payload)
        except json.JSONDecodeError as err:
            raise PerfParseError(f"invalid PERF_EVENT JSON: {payload!r}") from err
        if not isinstance(parsed, dict) or not parsed.get("phase"):
            raise PerfParseError(f"PERF_EVENT missing phase: {payload!r}")
        events.append(parsed)
    return events


def events_by_phase(events: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    grouped: dict[str, list[dict[str, Any]]] = {}
    for event in events:
        grouped.setdefault(str(event["phase"]), []).append(event)
    return grouped


def missing_phases(events: list[dict[str, Any]], required: list[str]) -> list[str]:
    present = {str(event["phase"]) for event in events}
    return [phase for phase in required if phase not in present]
