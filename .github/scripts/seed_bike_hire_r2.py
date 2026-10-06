#!/usr/bin/env python3
"""Generate 100_000 bike-hire JSON events and upload one gzip object to R2."""

from __future__ import annotations

import argparse
import gzip
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


EVENT_COUNT = 100_000
BUCKET = "skippr-e2e-sample-data"
OBJECT_KEY = "bike-hire/bikehire1.json.gz"
EVENT_TYPES = ("trip_start", "trip_pause", "trip_resume", "trip_end")


class SeedError(RuntimeError):
    pass


def event_for_index(index: int) -> dict[str, object]:
    event_type = EVENT_TYPES[(index - 1) % len(EVENT_TYPES)]
    started = datetime(2024, 1, 1, tzinfo=timezone.utc).timestamp() + index
    return {
        "rider_id": index,
        "bike_id": index,
        "event_type": event_type,
        "message_type": "bike_hire",
        "event_date": datetime.fromtimestamp(started, tz=timezone.utc).isoformat(),
        "isbn": f"isbn-{index}",
        "trip": {"id": index, "started_at": started},
        "last_crank": index % 360,
        "crank_torques": [index % 10, (index + 1) % 10],
        "hardware": {"firmware": "e2e"},
        "metadata": {"seed": "bike_hire_r2"},
    }


def write_gzip_jsonl(path: Path, *, count: int = EVENT_COUNT) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with gzip.open(path, "wt", encoding="utf-8") as handle:
        for index in range(1, count + 1):
            handle.write(json.dumps(event_for_index(index), separators=(",", ":")) + "\n")


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


def upload(path: Path, endpoint: str, env: dict[str, str]) -> None:
    result = subprocess.run(
        [
            "aws",
            "s3",
            "cp",
            str(path),
            f"s3://{BUCKET}/{OBJECT_KEY}",
            "--endpoint-url",
            endpoint,
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
    parser.add_argument("--count", type=int, default=EVENT_COUNT)
    args = parser.parse_args(argv)
    work = Path("/tmp/skippr-bike-hire-r2")
    archive = work / "bikehire1.json.gz"
    write_gzip_jsonl(archive, count=args.count)
    print(f"wrote {args.count} events to {archive}", file=sys.stderr)
    if args.skip_upload:
        return 0
    endpoint, env = r2_env()
    ensure_bucket(endpoint, env)
    upload(archive, endpoint, env)
    print(f"uploaded s3://{BUCKET}/{OBJECT_KEY}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SeedError as err:
        print(f"seed failed: {err}", file=sys.stderr)
        raise SystemExit(1)
