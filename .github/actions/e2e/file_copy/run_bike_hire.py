#!/usr/bin/env python3
"""Local bike_hire JSON → partitioned jsonl File sink, plus json → csv nested cells."""

from __future__ import annotations

import csv
import io
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

ACTION = Path(__file__).resolve().parent


def load_records(path: Path) -> list[dict]:
    text = path.read_text()
    records = []
    stripped = text.strip()
    if stripped.startswith("["):
        payload = json.loads(stripped)
        if not isinstance(payload, list):
            raise SystemExit(f"{path}: JSON array expected")
        records = [row for row in payload if isinstance(row, dict)]
    else:
        for line in text.splitlines():
            if not line.strip():
                continue
            row = json.loads(line)
            if isinstance(row, dict):
                records.append(row)
    if not records:
        raise SystemExit(f"{path}: no JSON objects")
    return records


def partition_field(records: list[dict]) -> str:
    preferred = (
        "event_type",
        "start_station_name",
        "end_station_name",
        "region",
    )
    for name in preferred:
        values = {str(row.get(name, "")).strip() for row in records if row.get(name) not in (None, "")}
        if len(values) >= 2:
            return name
    for key, value in records[0].items():
        if isinstance(value, str) and value:
            others = {str(row.get(key, "")) for row in records if isinstance(row.get(key), str)}
            if len(others) >= 2:
                return key
    raise SystemExit("bike_hire JSON has no string field with ≥2 values to partition on")


def nested_keys(records: list[dict]) -> list[str]:
    keys = []
    for key, value in records[0].items():
        if isinstance(value, (dict, list)):
            keys.append(key)
    return keys


def run_sync(skipprd: str, cfg: Path, data: Path) -> None:
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
            "file_copy",
            "--once",
        ],
        env=env,
    )


def write_cfg(
    template: str,
    source: Path,
    out: Path,
    sink_fmt: str,
    partition: str | None,
) -> str:
    yaml = (
        template.replace("SOURCE_DIR_PLACEHOLDER", str(source))
        .replace("OUTPUT_DIR_PLACEHOLDER", str(out))
        .replace("format: json\n", "format: json\n")
        .replace("format: jsonl\n", f"format: {sink_fmt}\n")
    )
    if partition:
        yaml = yaml.replace(
            "      batch_partition_fields: region\n",
            f"      batch_partition_fields: {partition}\n",
        )
    else:
        yaml = yaml.replace("      batch_partition_fields: region\n", "")
    return yaml


def main() -> int:
    skipprd = os.environ["SKIPPRD_PATH"]
    work = Path(os.environ["COPY_WORK"]) / "bike_hire"
    fixture = Path(os.environ["BIKE_HIRE_JSON"])
    records = load_records(fixture)
    partition = partition_field(records)
    nested = nested_keys(records)
    print(f"bike_hire: {len(records)} records, partition={partition}, nested={nested}")

    template = (ACTION / "skippr.yml").read_text()
    source = work / "source"
    source.mkdir(parents=True, exist_ok=True)
    shutil.copy(fixture, source / fixture.name)

    jsonl_out = work / "jsonl" / "output"
    jsonl_data = work / "jsonl" / "data"
    jsonl_out.mkdir(parents=True, exist_ok=True)
    jsonl_data.mkdir(parents=True, exist_ok=True)
    jsonl_cfg = work / "jsonl" / "skippr.yml"
    jsonl_cfg.write_text(write_cfg(template, source, jsonl_out, "jsonl", partition))
    run_sync(skipprd, jsonl_cfg, jsonl_data)
    jsonl_files = list(jsonl_out.rglob("*.jsonl"))
    if not jsonl_files:
        print(f"bike_hire jsonl: no objects under {jsonl_out}", file=sys.stderr)
        return 1
    joined = " ".join(str(path) for path in jsonl_files)
    values = {str(row.get(partition, "")).strip().lower() for row in records if row.get(partition) not in (None, "")}
    missing = [value for value in values if value and value not in joined.lower()]
    if missing:
        print(
            f"bike_hire jsonl: expected partition values {missing} in {jsonl_files}",
            file=sys.stderr,
        )
        return 1
    written = 0
    for path in jsonl_files:
        written += sum(1 for line in path.read_text().splitlines() if line.strip())
    if written != len(records):
        print(
            f"bike_hire jsonl: expected {len(records)} records, got {written}",
            file=sys.stderr,
        )
        return 1
    print(f"bike_hire jsonl: {len(jsonl_files)} object(s), {written} records, partitioned by {partition}")

    csv_out = work / "csv" / "output"
    csv_data = work / "csv" / "data"
    csv_out.mkdir(parents=True, exist_ok=True)
    csv_data.mkdir(parents=True, exist_ok=True)
    csv_cfg = work / "csv" / "skippr.yml"
    csv_cfg.write_text(write_cfg(template, source, csv_out, "csv", None))
    run_sync(skipprd, csv_cfg, csv_data)
    csv_files = list(csv_out.rglob("*.csv"))
    if not csv_files:
        print(f"bike_hire csv: no objects under {csv_out}", file=sys.stderr)
        return 1
    rows = []
    for path in csv_files:
        rows.extend(list(csv.DictReader(io.StringIO(path.read_text()))))
    if len(rows) != len(records):
        print(
            f"bike_hire csv: expected {len(records)} rows, got {len(rows)} in {csv_files}",
            file=sys.stderr,
        )
        return 1
    if nested:
        sample = rows[0]
        for key in nested:
            raw = sample.get(key)
            if raw is None:
                print(f"bike_hire csv: missing nested column {key}", file=sys.stderr)
                return 1
            try:
                parsed = json.loads(raw)
            except json.JSONDecodeError as err:
                print(
                    f"bike_hire csv: {key} is not JSON ({raw!r}): {err}",
                    file=sys.stderr,
                )
                return 1
            if not isinstance(parsed, (dict, list)):
                print(
                    f"bike_hire csv: {key} should be object/array JSON, got {parsed!r}",
                    file=sys.stderr,
                )
                return 1
        print(f"bike_hire csv: {len(rows)} rows, nested columns {nested} are JSON strings")
    else:
        print(f"bike_hire csv: {len(rows)} flat rows (no nested fields in sample)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
