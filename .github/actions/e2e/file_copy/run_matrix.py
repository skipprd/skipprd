#!/usr/bin/env python3
"""File → File copy matrix: json/csv/xml × jsonl/csv/xml plus nested json/xml → csv."""

from __future__ import annotations

import csv
import io
import json
import os
import shutil
import subprocess
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

ACTION = Path(__file__).resolve().parent
sys.path.insert(0, str(ACTION.parents[2] / "scripts"))
from skipprd_cli import log_args  # noqa: E402

TESTDATA = ACTION.parent / "s3_file_copy" / "testdata"
CELLS = [
    ("json", "jsonl", TESTDATA / "flat.json", True),
    ("json", "csv", TESTDATA / "flat.json", True),
    ("json", "xml", TESTDATA / "flat.json", True),
    ("csv", "jsonl", TESTDATA / "flat.csv", True),
    ("csv", "csv", TESTDATA / "flat.csv", True),
    ("csv", "xml", TESTDATA / "flat.csv", True),
    ("xml", "jsonl", TESTDATA / "flat.xml", True),
    ("xml", "csv", TESTDATA / "flat.xml", True),
    ("xml", "xml", TESTDATA / "flat.xml", True),
    ("json", "jsonl", TESTDATA / "nested.json", False),
    ("json", "csv", TESTDATA / "nested.json", False),
    ("xml", "csv", TESTDATA / "nested.xml", False),
]


def count_records(path: Path, sink_fmt: str) -> int:
    text = path.read_text()
    if sink_fmt == "jsonl":
        return sum(1 for line in text.splitlines() if line.strip())
    if sink_fmt == "csv":
        return len(list(csv.DictReader(io.StringIO(text))))
    if sink_fmt == "xml":
        return len(list(ET.fromstring(text)))
    raise ValueError(sink_fmt)


def record_ids(path: Path, sink_fmt: str) -> set[str]:
    text = path.read_text()
    if sink_fmt == "jsonl":
        return {
            str(json.loads(line)["id"])
            for line in text.splitlines()
            if line.strip()
        }
    if sink_fmt == "csv":
        return {row["id"] for row in csv.DictReader(io.StringIO(text))}
    if sink_fmt == "xml":
        ids = set()
        for child in ET.fromstring(text):
            node = child.find("id")
            if node is not None and node.text:
                ids.add(node.text)
        return ids
    raise ValueError(sink_fmt)


def assert_nested_csv(paths: list[Path]) -> str | None:
    seen = []
    for path in paths:
        for row in csv.DictReader(io.StringIO(path.read_text())):
            raw = row.get("detail")
            if not raw:
                return f"{path}: missing detail column in {row}"
            try:
                detail = json.loads(raw)
            except json.JSONDecodeError as err:
                return f"{path}: detail is not JSON ({raw!r}): {err}"
            if not isinstance(detail, dict) or "region" not in detail:
                return f"{path}: expected nested JSON object with region, got {detail!r}"
            seen.append(str(detail["region"]))
            scalars = {k: v for k, v in row.items() if k != "detail"}
            for key, value in scalars.items():
                if value.startswith("{") or value.startswith("["):
                    return f"{path}: top-level {key} looks nested ({value!r})"
    if sorted(seen) != ["eu", "us"]:
        return f"expected nested regions eu and us, got {seen}"
    return None


def run_cell(
    template: str,
    skipprd: str,
    work: Path,
    source_fmt: str,
    sink_fmt: str,
    fixture: Path,
    expect_region_partition: bool,
    cell: str,
) -> int:
    source = work / cell / "source"
    out = work / cell / "output"
    data = work / cell / "data"
    source.mkdir(parents=True, exist_ok=True)
    out.mkdir(parents=True, exist_ok=True)
    data.mkdir(parents=True, exist_ok=True)
    shutil.copy(fixture, source / fixture.name)
    yaml = (
        template.replace("SOURCE_DIR_PLACEHOLDER", str(source))
        .replace("OUTPUT_DIR_PLACEHOLDER", str(out))
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
            *log_args(env),
            "--pipeline",
            "file_copy",
            "--once",
        ],
        env=env,
    )
    written = list(out.rglob(f"*.{sink_fmt}"))
    if not written:
        print(f"{cell}: no {sink_fmt} objects under {out}", file=sys.stderr)
        return 1
    rows = sum(count_records(path, sink_fmt) for path in written)
    if rows != 2:
        print(f"{cell}: expected 2 records, got {rows} in {written}", file=sys.stderr)
        return 1
    ids = set().union(*(record_ids(path, sink_fmt) for path in written))
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
        err = assert_nested_csv(written)
        if err:
            print(f"{cell}: {err}", file=sys.stderr)
            return 1
    print(f"{cell}: {len(written)} object(s), {rows} records")
    return 0


def main() -> int:
    skipprd = os.environ["SKIPPRD_PATH"]
    work = Path(os.environ["COPY_WORK"])
    template = (ACTION / "skippr.yml").read_text()
    for source_fmt, sink_fmt, fixture, partitioned in CELLS:
        cell = f"{source_fmt}_{sink_fmt}_{fixture.stem}"
        if run_cell(
            template,
            skipprd,
            work,
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
