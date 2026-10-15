#!/usr/bin/env python3
"""File → S3 (R2) copy matrix: json/csv/xml × jsonl/csv/xml plus nested json/xml → csv."""

from __future__ import annotations

import importlib.util
import os
import shutil
import subprocess
import sys
from pathlib import Path

import boto3
from botocore.config import Config as BotoConfig

ACTION = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("s3_file_copy_matrix", ACTION / "run_matrix.py")
assert SPEC is not None and SPEC.loader is not None
matrix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(matrix)
CELLS = matrix.CELLS


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
    prefix = f"{prefix_root}{cell}/"
    source = work / cell / "source"
    data = work / cell / "data"
    downloaded = work / cell / "downloaded"
    source.mkdir(parents=True, exist_ok=True)
    data.mkdir(parents=True, exist_ok=True)
    shutil.copy(fixture, source / fixture.name)
    yaml = (
        template.replace("S3_PREFIX_PLACEHOLDER", prefix)
        .replace("R2_S3_ENDPOINT_PLACEHOLDER", endpoint)
        .replace("SOURCE_DIR_PLACEHOLDER", str(source))
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
    subprocess.check_call(
        [
            skipprd,
            "--config",
            str(cfg),
            "sync",
            "--log",
            "--pipeline",
            "file_s3_copy",
            "--once",
        ],
        env=env,
    )
    written = download_cell(client, bucket, prefix, downloaded, sink_fmt)
    if not written:
        print(f"{cell}: no {sink_fmt} objects under s3://{bucket}/{prefix}", file=sys.stderr)
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
            line + b"\n"
            for line in fixture.read_bytes().splitlines()
            if line.strip()
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
    template = (ACTION / "skippr_s3_sink.yml").read_text()
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
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
