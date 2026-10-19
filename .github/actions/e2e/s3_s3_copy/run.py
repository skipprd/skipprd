#!/usr/bin/env python3
"""S3 → S3 (R2) copy matrix: json/csv/xml × jsonl/csv/xml plus nested json/xml → csv.

This is the CloudCycle COPY shape (source objects, Identity PUT sink, optional
re-partition). It starts both S3 runtime plugins so a HostFrame/PrepareSink
mismatch fails the job (`runtime sink control frame read failed`).
"""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
from pathlib import Path

import boto3
from botocore.config import Config as BotoConfig

ACTION = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location(
    "s3_file_copy_matrix", ACTION.parent / "s3_file_copy" / "run_matrix.py"
)
assert SPEC is not None and SPEC.loader is not None
matrix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(matrix)
CELLS = matrix.CELLS
CONTROL_FRAME_ERROR = "runtime sink control frame read failed"
DESYNC_MARKERS = (
    CONTROL_FRAME_ERROR,
    "expected variant index",
    "skipped length prefix",
    "payload_len=6",
)
BURST_FILES = 16
NAMESPACE_TRANSFORM = """      flatten_events: false
      namespace_fields: detail-type
      batch_time_fields: time
      batch_time_unit: day
      time_partition_prefix: p_
      batch_partition_fields: detail.rmc
"""


def fail_if_control_desync(combined: str, cell: str) -> bool:
    for marker in DESYNC_MARKERS:
        if marker in combined:
            print(f"{cell}: control-frame regression: {marker}", file=sys.stderr)
            return True
    return False


def cube_event_object(index: int) -> bytes:
    namespace = f"EventType{index:02d}"
    rmc = f"rmc{index:02d}"
    records = [
        {
            "id": str(index),
            "detail-type": namespace,
            "time": "2026-01-15T12:00:00Z",
            "detail": {"rmc": rmc},
        },
        {
            "id": str(index + 1000),
            "detail-type": namespace,
            "time": "2026-01-15T13:00:00Z",
            "detail": {"rmc": rmc},
        },
    ]
    return "".join(json.dumps(record) + "\n" for record in records).encode()


def download_cell(client, bucket: str, prefix: str, dest: Path, sink_fmt: str) -> list[Path]:
    dest.mkdir(parents=True, exist_ok=True)
    written: list[Path] = []
    paginator = client.get_paginator("list_objects_v2")
    for page in paginator.paginate(Bucket=bucket, Prefix=prefix):
        for obj in page.get("Contents") or []:
            key = obj["Key"]
            if not key.endswith(f".{sink_fmt}"):
                continue
            relative = key[len(prefix) :].lstrip("/")
            path = dest / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            client.download_file(bucket, key, str(path))
            written.append(path)
    return written


def run_cell(
    client,
    bucket: str,
    template: str,
    skipprd: str,
    work: Path,
    prefix_root: str,
    endpoint: str,
    source_fmt: str,
    sink_fmt: str,
    fixture: Path,
    expect_region_partition: bool,
    cell: str,
) -> int:
    source_prefix = f"{prefix_root}{cell}/src/"
    sink_prefix = f"{prefix_root}{cell}/dst/"
    client.upload_file(str(fixture), bucket, f"{source_prefix}{fixture.name}")
    data = work / cell / "data"
    downloaded = work / cell / "downloaded"
    data.mkdir(parents=True, exist_ok=True)
    yaml = (
        template.replace("SOURCE_PREFIX_PLACEHOLDER", source_prefix)
        .replace("SINK_PREFIX_PLACEHOLDER", sink_prefix)
        .replace("R2_S3_ENDPOINT_PLACEHOLDER", endpoint)
        .replace("format: json\n", f"format: {source_fmt}\n")
        .replace("format: jsonl\n", f"format: {sink_fmt}\n")
    )
    if not expect_region_partition:
        yaml = yaml.replace("      batch_partition_fields: region\n", "")
    cfg = work / cell / "skippr.yml"
    cfg.write_text(yaml)
    env = os.environ.copy()
    env["DATA_DIR"] = str(data)
    env["DATA_DIR_MIN_FREE_BYTES"] = "0"
    env["DATA_DIR_HIGH_WATERMARK_PCT"] = "0"
    proc = subprocess.run(
        [
            skipprd,
            "--config",
            str(cfg),
            "sync",
            "--log",
            "--pipeline",
            "s3_s3_copy",
            "--once",
        ],
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.stdout:
        print(proc.stdout, end="")
    if proc.stderr:
        print(proc.stderr, end="", file=sys.stderr)
    combined = f"{proc.stdout}\n{proc.stderr}"
    if fail_if_control_desync(combined, cell):
        return 1
    if proc.returncode:
        print(f"{cell}: skipprd sync exited {proc.returncode}", file=sys.stderr)
        return proc.returncode
    written = download_cell(client, bucket, sink_prefix, downloaded, sink_fmt)
    if not written:
        print(f"{cell}: no {sink_fmt} objects under s3://{bucket}/{sink_prefix}", file=sys.stderr)
        return 1
    rows = sum(matrix.count_records(path, sink_fmt) for path in written)
    if rows != 2:
        print(f"{cell}: expected 2 records, got {rows} in {written}", file=sys.stderr)
        return 1
    ids = set().union(*(matrix.record_ids(path, sink_fmt) for path in written))
    if ids != {"1", "2"}:
        print(f"{cell}: expected ids 1 and 2, got {ids}", file=sys.stderr)
        return 1
    if expect_region_partition:
        joined = " ".join(str(path) for path in written)
        if "eu" not in joined or "us" not in joined:
            print(f"{cell}: expected region partitions in {written}", file=sys.stderr)
            return 1
    if source_fmt == "json" and sink_fmt == "jsonl":
        expect = [
            line + b"\n" for line in fixture.read_bytes().splitlines() if line.strip()
        ]
        remaining = list(expect)
        for path in written:
            body = path.read_bytes()
            if body and not body.endswith(b"\n"):
                print(f"{cell}: {path} missing trailing newline", file=sys.stderr)
                return 1
            cursor = body
            while cursor:
                match = next((rec for rec in remaining if cursor.startswith(rec)), None)
                if match is None:
                    print(
                        f"{cell}: {path} is not original NDJSON identity bytes",
                        file=sys.stderr,
                    )
                    return 1
                remaining.remove(match)
                cursor = cursor[len(match) :]
        if remaining:
            print(f"{cell}: missing identity records {remaining!r}", file=sys.stderr)
            return 1
    if sink_fmt == "csv" and fixture.stem == "nested":
        err = matrix.assert_nested_csv(written)
        if err:
            print(f"{cell}: {err}", file=sys.stderr)
            return 1
    print(f"{cell}: {len(written)} object(s), {rows} records")
    return 0


def run_namespace_burst(
    client,
    bucket: str,
    template: str,
    skipprd: str,
    work: Path,
    prefix_root: str,
    endpoint: str,
) -> int:
    """CloudCycle COPY shape: many namespaces, then Identity PUT PrepareSinks."""
    cell = "json_jsonl_namespace_burst"
    source_prefix = f"{prefix_root}{cell}/src/"
    sink_prefix = f"{prefix_root}{cell}/dst/"
    expected_rows = BURST_FILES * 2
    local_src = work / cell / "src"
    local_src.mkdir(parents=True, exist_ok=True)
    for index in range(BURST_FILES):
        body = cube_event_object(index)
        name = f"event-{index:02d}.json"
        path = local_src / name
        path.write_bytes(body)
        client.upload_file(str(path), bucket, f"{source_prefix}{name}")
    yaml = (
        template.replace("SOURCE_PREFIX_PLACEHOLDER", source_prefix)
        .replace("SINK_PREFIX_PLACEHOLDER", sink_prefix)
        .replace("R2_S3_ENDPOINT_PLACEHOLDER", endpoint)
        .replace("      batch_partition_fields: region\n", NAMESPACE_TRANSFORM)
    )
    cfg = work / cell / "skippr.yml"
    cfg.write_text(yaml)
    data = work / cell / "data"
    downloaded = work / cell / "downloaded"
    data.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["DATA_DIR"] = str(data)
    env["DATA_DIR_MIN_FREE_BYTES"] = "0"
    env["DATA_DIR_HIGH_WATERMARK_PCT"] = "0"
    proc = subprocess.run(
        [
            skipprd,
            "--config",
            str(cfg),
            "sync",
            "--log",
            "--pipeline",
            "s3_s3_copy",
            "--once",
        ],
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    if proc.stdout:
        print(proc.stdout, end="")
    if proc.stderr:
        print(proc.stderr, end="", file=sys.stderr)
    combined = f"{proc.stdout}\n{proc.stderr}"
    if fail_if_control_desync(combined, cell):
        return 1
    if proc.returncode:
        print(f"{cell}: skipprd sync exited {proc.returncode}", file=sys.stderr)
        return proc.returncode
    written = download_cell(client, bucket, sink_prefix, downloaded, "jsonl")
    if not written:
        print(f"{cell}: no jsonl objects under s3://{bucket}/{sink_prefix}", file=sys.stderr)
        return 1
    rows = sum(matrix.count_records(path, "jsonl") for path in written)
    if rows != expected_rows:
        print(f"{cell}: expected {expected_rows} records, got {rows} in {written}", file=sys.stderr)
        return 1
    print(f"{cell}: {len(written)} object(s), {rows} records")
    return 0


def main() -> int:
    skipprd = os.environ["SKIPPRD_PATH"]
    work = Path(os.environ["COPY_WORK"])
    endpoint = os.environ["R2_S3_ENDPOINT"]
    prefix_root = os.environ["COPY_S3_PREFIX"].rstrip("/") + "/"
    bucket = "skippr-e2e-sample-data"
    client = boto3.client(
        "s3",
        endpoint_url=endpoint,
        region_name="auto",
        aws_access_key_id=os.environ["AWS_ACCESS_KEY_ID"],
        aws_secret_access_key=os.environ["AWS_SECRET_ACCESS_KEY"],
        config=BotoConfig(retries={"max_attempts": 8}),
    )
    template = (ACTION / "skippr.yml").read_text()
    for source_fmt, sink_fmt, fixture, partitioned in CELLS:
        cell = f"{source_fmt}_{sink_fmt}_{fixture.stem}"
        if run_cell(
            client,
            bucket,
            template,
            skipprd,
            work,
            prefix_root,
            endpoint,
            source_fmt,
            sink_fmt,
            fixture,
            partitioned,
            cell,
        ):
            return 1
    if run_namespace_burst(
        client, bucket, template, skipprd, work, prefix_root, endpoint
    ):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
