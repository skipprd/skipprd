#!/usr/bin/env python3
"""Generate 5_100_000 mixed-size bike-hire JSON objects and upload them to R2."""

from __future__ import annotations

import argparse
import gzip
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterable, Literal, NamedTuple


CANONICAL_EVENT_COUNT = 5_100_000
EVOLVE_EVENT_COUNT = 2_000
BUCKET = "skippr-e2e-sample-data"
CHAOS_PREFIX = "bike-hire/"
EVOLVE_PREFIX = "bike-hire-evolve/"
EVENT_TYPES = ("trip_start", "trip_pause", "trip_resume", "trip_end")
Shape = Literal["v1", "v2", "flat"]


class ObjectSpec(NamedTuple):
    relative_key: str
    event_count: int
    shape: Shape
    gzip: bool


class SeedError(RuntimeError):
    pass


def _bulk_specs() -> tuple[ObjectSpec, ...]:
    return tuple(
        ObjectSpec(f"bulk/part-{index:02d}.json.gz", 480_000, "v1", True)
        for index in range(10)
    )


def _medium_specs() -> tuple[ObjectSpec, ...]:
    return tuple(
        ObjectSpec(f"medium/m-{index:03d}.json.gz", 1_000, "flat", True)
        for index in range(200)
    )


def _tiny_specs() -> tuple[ObjectSpec, ...]:
    specs: list[ObjectSpec] = []
    for index in range(1_000):
        year = 2020 if index < 500 else 2021
        specs.append(
            ObjectSpec(
                f"tiny/{year}/01/01/t-{index:04d}.json",
                50,
                "flat",
                False,
            )
        )
    return tuple(specs)


CHAOS_SPECS: tuple[ObjectSpec, ...] = (
    *_bulk_specs(),
    *_medium_specs(),
    *_tiny_specs(),
    ObjectSpec("evolve/v1.json.gz", 25_000, "v1", True),
    ObjectSpec("evolve/v2.json.gz", 25_000, "v2", True),
)

EVOLVE_SPECS: tuple[ObjectSpec, ...] = (
    ObjectSpec("v1/events.json.gz", 1_000, "v1", True),
    ObjectSpec("v2/events.json.gz", 1_000, "v2", True),
)


def event_total(specs: Iterable[ObjectSpec]) -> int:
    return sum(spec.event_count for spec in specs)


def event_for_index(index: int, shape: Shape) -> dict[str, object]:
    event_type = EVENT_TYPES[(index - 1) % len(EVENT_TYPES)]
    started = datetime(2024, 1, 1, tzinfo=timezone.utc).timestamp() + index
    event_date = datetime.fromtimestamp(started, tz=timezone.utc).isoformat()
    if shape == "flat":
        return {
            "rider_id": index,
            "bike_id": index,
            "event_type": event_type,
            "message_type": "bike_hire",
            "event_date": event_date,
            "isbn": f"isbn-{index}",
            "trip_id": index,
            "trip_started_at": started,
            "last_crank": index % 360,
            "firmware": "e2e",
            "metadata_seed": "bike_hire_r2",
        }
    hardware: dict[str, object] = {"firmware": "e2e"}
    event: dict[str, object] = {
        "rider_id": index,
        "bike_id": index,
        "event_type": event_type,
        "message_type": "bike_hire",
        "event_date": event_date,
        "isbn": f"isbn-{index}",
        "trip": {"id": index, "started_at": started},
        "last_crank": index % 360,
        "crank_torques": [index % 10, (index + 1) % 10],
        "hardware": hardware,
        "metadata": {"seed": "bike_hire_r2", "schema": 1 if shape == "v1" else 2},
    }
    if shape == "v2":
        event["firmware_revision"] = "2.0"
        hardware["modem"] = "lte"
    return event


def _open_writer(path: Path, gzip_enabled: bool):
    path.parent.mkdir(parents=True, exist_ok=True)
    if gzip_enabled:
        return gzip.open(path, "wt", encoding="utf-8")
    return path.open("w", encoding="utf-8")


def write_specs(root: Path, specs: tuple[ObjectSpec, ...] | list[ObjectSpec]) -> list[Path]:
    written: list[Path] = []
    next_id = 1
    for spec in specs:
        path = root / spec.relative_key
        with _open_writer(path, spec.gzip) as handle:
            for _ in range(spec.event_count):
                handle.write(
                    json.dumps(event_for_index(next_id, spec.shape), separators=(",", ":"))
                    + "\n"
                )
                next_id += 1
        written.append(path)
    return written


def r2_env() -> tuple[str, dict[str, str]]:
    account = os.environ.get("R2_ACCOUNT_ID") or os.environ.get("CLOUDFLARE_ACCOUNT_ID") or ""
    access = (
        os.environ.get("R2_ACCESS_KEY_ID")
        or os.environ.get("OBJECTS_ACCESS_KEY_ID")
        or ""
    )
    secret = (
        os.environ.get("R2_SECRET_ACCESS_KEY")
        or os.environ.get("OBJECTS_SECRET_ACCESS_KEY")
        or ""
    )
    endpoint = os.environ.get("OBJECTS_S3_ENDPOINT") or (
        f"https://{account}.r2.cloudflarestorage.com" if account else ""
    )
    if not (endpoint and access and secret):
        raise SeedError(
            "set R2_ACCOUNT_ID (or CLOUDFLARE_ACCOUNT_ID), R2_ACCESS_KEY_ID, and "
            "R2_SECRET_ACCESS_KEY (or OBJECTS_* equivalents)"
        )
    env = os.environ.copy()
    env["AWS_ACCESS_KEY_ID"] = access
    env["AWS_SECRET_ACCESS_KEY"] = secret
    env.pop("AWS_SESSION_TOKEN", None)
    env["AWS_DEFAULT_REGION"] = os.environ.get("OBJECTS_S3_REGION") or "auto"
    return endpoint, env


def ensure_bucket(endpoint: str, env: dict[str, str]) -> None:
    listed = subprocess.run(
        ["aws", "s3", "ls", f"s3://{BUCKET}", "--endpoint-url", endpoint],
        env=env,
        capture_output=True,
        text=True,
    )
    if listed.returncode == 0:
        return
    created = subprocess.run(
        ["aws", "s3", "mb", f"s3://{BUCKET}", "--endpoint-url", endpoint],
        env=env,
        capture_output=True,
        text=True,
    )
    if created.returncode != 0:
        raise SeedError(created.stderr.strip() or created.stdout.strip())


def sync_prefix(local: Path, prefix: str, endpoint: str, env: dict[str, str]) -> None:
    result = subprocess.run(
        [
            "aws",
            "s3",
            "sync",
            str(local),
            f"s3://{BUCKET}/{prefix}",
            "--endpoint-url",
            endpoint,
            "--delete",
        ],
        env=env,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise SeedError(result.stderr.strip() or result.stdout.strip())


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--skip-upload", action="store_true")
    args = parser.parse_args(argv)
    if event_total(CHAOS_SPECS) != CANONICAL_EVENT_COUNT:
        raise SeedError(
            f"chaos layout totals {event_total(CHAOS_SPECS)}, expected {CANONICAL_EVENT_COUNT}"
        )
    work = Path("/tmp/skippr-bike-hire-r2")
    chaos_root = work / "chaos"
    evolve_root = work / "evolve"
    if chaos_root.exists():
        subprocess.run(["rm", "-rf", str(chaos_root)], check=True)
    if evolve_root.exists():
        subprocess.run(["rm", "-rf", str(evolve_root)], check=True)
    write_specs(chaos_root, CHAOS_SPECS)
    write_specs(evolve_root, EVOLVE_SPECS)
    print(
        f"wrote {event_total(CHAOS_SPECS)} chaos events and "
        f"{event_total(EVOLVE_SPECS)} evolve events under {work}",
        file=sys.stderr,
    )
    if args.skip_upload:
        return 0
    endpoint, env = r2_env()
    ensure_bucket(endpoint, env)
    sync_prefix(chaos_root, CHAOS_PREFIX, endpoint, env)
    sync_prefix(evolve_root, EVOLVE_PREFIX, endpoint, env)
    print(
        f"uploaded s3://{BUCKET}/{CHAOS_PREFIX} and s3://{BUCKET}/{EVOLVE_PREFIX}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SeedError as err:
        print(f"seed failed: {err}", file=sys.stderr)
        raise SystemExit(1)
