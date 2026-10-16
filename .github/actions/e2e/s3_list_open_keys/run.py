#!/usr/bin/env python3
"""Local e2e: File copy starts from open objects and skips Closed offsets.

Does not resume S3 continuation tokens. The second pass must not rewrite
already-copied objects; a newly added source object is copied on the next run.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

ACTION = Path(__file__).resolve().parent


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
            "file_copy",
            "--once",
        ],
        env=env,
    )


def main() -> int:
    skipprd = os.environ["SKIPPRD_PATH"]
    work = Path(os.environ["COPY_WORK"])
    source = work / "source"
    out = work / "output"
    data = work / "data"
    for path in (source, out, data):
        shutil.rmtree(path, ignore_errors=True)
        path.mkdir(parents=True)
    write_jsonl(
        source / "a.json",
        [{"id": "1", "region": "eu"}, {"id": "2", "region": "us"}],
    )
    template = (ACTION / "skippr.yml").read_text()
    yaml = template.replace("SOURCE_DIR_PLACEHOLDER", str(source)).replace(
        "OUTPUT_DIR_PLACEHOLDER", str(out)
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
    write_jsonl(source / "b.json", [{"id": "3", "region": "eu"}])
    sync(skipprd, cfg, data)
    third = copied_ids(out)
    if third != {"1", "2", "3"}:
        print(f"third pass expected ids 1, 2, 3, got {third}", file=sys.stderr)
        return 1
    print(f"open-keys copy: {sorted(third)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
