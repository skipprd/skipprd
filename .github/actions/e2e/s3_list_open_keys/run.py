#!/usr/bin/env python3
"""Local e2e: S3 copy starts GET for open keys and skips Closed offsets.

Each run re-lists the prefix. It does not resume from a continuation token.
The second pass must not rewrite already-copied objects; a newly listed object
is copied on the next run.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import boto3
from botocore.config import Config as BotoConfig

ACTION = Path(__file__).resolve().parent
BUCKET = "skippr-e2e-sample-data"


def write_jsonl(path: Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(row) + "\n" for row in rows))


def copied_ids(out: Path) -> set[str]:
    ids: set[str] = set()
    for path in out.rglob("*.jsonl"):
        for line in path.read_text().splitlines():
            if line.strip():
                ids.add(str(json.loads(line)["id"]))
    return ids


def sync(skipprd: str, cfg: Path, data: Path) -> None:
    env = os.environ.copy()
    env["DATA_DIR"] = str(data)
    env["DATA_DIR_MIN_FREE_BYTES"] = "0"
    env["DATA_DIR_HIGH_WATERMARK_PCT"] = "0"
    subprocess.check_call(
        [
            skipprd,
            "--config",
            str(cfg),
            "sync",
            "--log",
            "--pipeline",
            "s3_file_copy",
            "--once",
        ],
        env=env,
    )


def main() -> int:
    skipprd = os.environ["SKIPPRD_PATH"]
    work = Path(os.environ["COPY_WORK"])
    endpoint = os.environ["R2_S3_ENDPOINT"]
    prefix = os.environ["COPY_S3_PREFIX"].rstrip("/") + "/"
    out = work / "output"
    data = work / "data"
    staging = work / "staging"
    for path in (out, data, staging):
        shutil.rmtree(path, ignore_errors=True)
        path.mkdir(parents=True)
    client = boto3.client(
        "s3",
        endpoint_url=endpoint,
        region_name="auto",
        aws_access_key_id=os.environ["AWS_ACCESS_KEY_ID"],
        aws_secret_access_key=os.environ["AWS_SECRET_ACCESS_KEY"],
        config=BotoConfig(retries={"max_attempts": 8}),
    )
    a_json = staging / "a.json"
    b_json = staging / "b.json"
    write_jsonl(
        a_json,
        [{"id": "1", "region": "eu"}, {"id": "2", "region": "us"}],
    )
    write_jsonl(b_json, [{"id": "3", "region": "eu"}])
    client.upload_file(str(a_json), BUCKET, f"{prefix}a.json")
    yaml = (
        (ACTION / "skippr.yml")
        .read_text()
        .replace("S3_PREFIX_PLACEHOLDER", prefix)
        .replace("R2_S3_ENDPOINT_PLACEHOLDER", endpoint)
        .replace("OUTPUT_DIR_PLACEHOLDER", str(out))
    )
    cfg = work / "skippr.yml"
    cfg.write_text(yaml)
    sync(skipprd, cfg, data)
    first = copied_ids(out)
    if first != {"1", "2"}:
        print(f"first pass expected ids 1 and 2, got {first}", file=sys.stderr)
        return 1
    first_objects = {path.relative_to(out): path.read_bytes() for path in out.rglob("*.jsonl")}
    sync(skipprd, cfg, data)
    if copied_ids(out) != {"1", "2"}:
        print("second pass must not invent records", file=sys.stderr)
        return 1
    second_objects = {path.relative_to(out): path.read_bytes() for path in out.rglob("*.jsonl")}
    if second_objects != first_objects:
        print("second pass rewrote Closed objects", file=sys.stderr)
        return 1
    client.upload_file(str(b_json), BUCKET, f"{prefix}b.json")
    sync(skipprd, cfg, data)
    third = copied_ids(out)
    if third != {"1", "2", "3"}:
        print(f"third pass expected ids 1, 2, 3, got {third}", file=sys.stderr)
        return 1
    print(f"open-keys copy: {sorted(third)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
