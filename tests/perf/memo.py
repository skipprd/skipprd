#!/usr/bin/env python3
"""Plain-English skippr-perf memo for paul@paulhudson.me after each GitHub iteration."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path
from typing import Any

HARNESS_DIR = Path(__file__).resolve().parent
REPO_ROOT = HARNESS_DIR.parents[1]
CLOUD_SEND = REPO_ROOT.parent / "cloud" / "scripts" / "send-afk-mail.py"
MEMO_TO = "paul@paulhudson.me"

NOISE_LEAVES = {
    "read",
    "write",
    "libc.so.6",
    "ld-linux-x86-64.so.2",
    "__munmap",
    "__mprotect",
    "statx",
    "execve",
    "_Fork",
    "open64",
    "mkdir",
    "next",
    "rotate_left",
    "atomic_load<usize>",
}


def preferred_reports(artifacts: Path) -> dict[str, dict[str, Any]]:
    chosen: dict[str, tuple[str, dict[str, Any]]] = {}
    for path in artifacts.rglob("perf-report.json"):
        payload = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(payload, dict):
            continue
        scenario = str(payload.get("scenario") or "").strip()
        if not scenario:
            continue
        rel = str(path.relative_to(artifacts))
        prev = chosen.get(scenario)
        if prev is None or len(rel) < len(prev[0]):
            chosen[scenario] = (rel, payload)
    return {name: item[1] for name, item in chosen.items()}


INGEST_LABELS = {
    "ingest-a-duckdb": "Ingest-A",
    "ingest-csv-duckdb": "Skipprd CSV to DuckDB",
    "copy-b-json-json": "Copy-B",
    "ingest-json-parquet": "JSON File to Parquet File",
    "duckdb-ndjson": "DuckDB NDJSON (fast)",
    "duckdb-csv": "DuckDB CSV COPY (fast)",
    "duckdb-json-array": "DuckDB nested JSON array (slow)",
    "duckdb-insert-values": "DuckDB row INSERT (slow)",
    "ingest-slow-flatten-duckdb": "Slow flatten",
}

QUERY_LABELS = {
    "q-wal-count-iceberg": "WAL+file Iceberg count",
    "q-wal-point-iceberg": "WAL+file Iceberg point",
    "q-union-count-duckdb": "DuckDB UNION count",
    "q-iceberg-only-iceberg": "File Iceberg-only count",
    "q-iceberg-only-duckdb": "DuckDB Iceberg-only count",
}


def memo_suite(reports: dict[str, dict[str, Any]]) -> str:
    if any(payload.get("kind") == "query_after_ingest" for payload in reports.values()):
        return "skippr-perf-db"
    return "skippr-perf"


def timings_from_reports(reports: dict[str, dict[str, Any]]) -> list[str]:
    lines: list[str] = []
    for name, label in INGEST_LABELS.items():
        payload = reports.get(name)
        if not payload or payload.get("error"):
            continue
        rows = payload.get("rows")
        elapsed = payload.get("elapsed_ms")
        if rows is None or elapsed is None:
            continue
        line = f"{label} synced {rows:,} rows in {elapsed}ms."
        exact = payload.get("exact_arrow_rows")
        if name in {"ingest-a-duckdb", "ingest-json-parquet"} and exact is not None:
            line += f" Exact Arrow kept {exact:,} of those rows."
        spawn = payload.get("plugin_spawn_ms")
        apply = payload.get("plugin_apply_ms")
        if spawn is not None or apply is not None:
            line += f" Plugin spawn {spawn or 0}ms, sink apply {apply or 0}ms."
        lines.append(line)
    for name, label in QUERY_LABELS.items():
        payload = reports.get(name)
        if not payload or payload.get("error"):
            continue
        rows = payload.get("rows")
        elapsed = payload.get("query_collect_ms")
        if elapsed is None:
            elapsed = payload.get("elapsed_ms")
        if rows is None or elapsed is None:
            continue
        lines.append(f"{label} queried {rows:,} ingest rows in {elapsed}ms.")
    return lines


def ideas_from_report(
    bottlenecks: dict[str, Any],
    reports: dict[str, dict[str, Any]],
) -> list[str]:
    ideas: list[str] = []
    seen: set[str] = set()

    def add(idea: str) -> None:
        if idea not in seen:
            seen.add(idea)
            ideas.append(idea)

    for name, payload in sorted(reports.items()):
        error = str(payload.get("error") or "").strip()
        if not error:
            continue
        if "file_s3_copy" in error or name == "copy-file-s3":
            add("copy-file-s3 used the S3-to-file config; point it at skippr_s3_sink.yml.")
        elif name == "copy-stdin-file" or "wal_persist" in error:
            add("copy-stdin-file finished with zero rows because the Stdin plugin is a separate process.")
        elif "timed out" in error.lower():
            add("A cell hung skipprd until the GitHub job timed out; cap the process and kill the group.")
        else:
            add(f"{name} failed: {error.splitlines()[0][:180]}")

    hottest_new = bottlenecks.get("summary", {}).get("hottest_new_code") or []
    if hottest_new and float(hottest_new[0].get("pct") or 0) < 1:
        add("New WAL catalog code is not the CPU bottleneck; do not spend a round on it.")

    for profile in bottlenecks.get("profiles") or []:
        for leaf in profile.get("hot_leaves") or []:
            symbol = str(leaf.get("symbol") or "")
            pct = float(leaf.get("pct") or 0)
            if pct < 10 or not symbol or symbol in NOISE_LEAVES:
                continue
            if symbol.startswith("_mm_"):
                continue
            if "sha2" in symbol.lower() or "sha256" in symbol.lower():
                add(
                    f"SHA-256 hashing is about {pct:.0f}% of ingest CPU. Check whether each batch is hashed more than once."
                )
            else:
                add(f"{symbol} is about {pct:.0f}% of samples in {profile.get('path')}.")

    return ideas


def broke_from_reports(reports: dict[str, dict[str, Any]]) -> list[str]:
    lines: list[str] = []
    for name, payload in sorted(reports.items()):
        error = str(payload.get("error") or "").strip()
        if not error:
            continue
        if name == "copy-file-s3" or "file_s3_copy" in error:
            lines.append(
                "File-to-S3 used the S3-to-file config, so the pipeline name did not match."
            )
        elif name == "copy-stdin-file":
            lines.append(
                "Stdin copy finished with zero rows. The Stdin plugin is a separate process, so piping into skipprd does not feed it."
            )
        elif "timed out" in error.lower():
            lines.append(
                "A cell never finished. skipprd had no timeout, so GitHub sat on it."
            )
        else:
            lines.append(f"{name} failed: {error.splitlines()[0][:160]}")
    return lines


def write_memo(
    *,
    iteration: int,
    run_url: str,
    timings: list[str],
    broke: list[str],
    done: list[str],
    nxt: list[str],
    suite: str = "skippr-perf",
) -> str:
    paragraphs = [
        f"Paul,",
        f"Iteration {iteration} of {suite} on GitHub Linux x86. This note is for {MEMO_TO}.",
    ]
    if timings:
        paragraphs.append("What the numbers said\n" + "\n".join(f"- {line}" for line in timings))
    if broke:
        paragraphs.append("What broke\n" + "\n".join(f"- {line}" for line in broke))
    else:
        paragraphs.append("What broke\n- No cell failed in the reports I have.")
    if done:
        paragraphs.append("What I changed this round\n" + "\n".join(f"- {line}" for line in done))
    if nxt:
        paragraphs.append("What is next\n" + "\n".join(f"- {line}" for line in nxt))
    else:
        paragraphs.append(
            "What is next\n- Ideas look exhausted: the smoke cells moved little, and no cell is still broken."
        )
    if run_url:
        paragraphs.append(f"GitHub run: {run_url}")
    return "\n\n".join(paragraphs).strip() + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Write a skippr-perf memo")
    parser.add_argument("--iteration", type=int, required=True)
    parser.add_argument("--artifacts", default="")
    parser.add_argument("--bottlenecks", default="")
    parser.add_argument("--run-url", default="")
    parser.add_argument("--done", action="append", default=[])
    parser.add_argument("--next", action="append", default=[])
    parser.add_argument("--out", default="skippr-perf-memo.txt")
    parser.add_argument("--mail", action="store_true")
    args = parser.parse_args(argv)

    reports: dict[str, dict[str, Any]] = {}
    bottlenecks: dict[str, Any] = {}
    if args.artifacts:
        reports = preferred_reports(Path(args.artifacts))
    if args.bottlenecks:
        bottlenecks = json.loads(Path(args.bottlenecks).read_text(encoding="utf-8"))

    timings = timings_from_reports(reports)
    broke = broke_from_reports(reports)
    ideas = ideas_from_report(bottlenecks, reports)
    nxt = list(args.next) or ideas
    suite = memo_suite(reports)
    body = write_memo(
        iteration=args.iteration,
        run_url=args.run_url,
        timings=timings,
        broke=broke,
        done=list(args.done),
        nxt=nxt,
        suite=suite,
    )
    out = Path(args.out)
    out.write_text(body, encoding="utf-8")
    print(body)
    print(f"wrote {out}", flush=True)
    if args.mail:
        if not CLOUD_SEND.is_file():
            raise SystemExit(f"missing AFK mail script {CLOUD_SEND}")
        subprocess.run(
            [
                sys.executable,
                str(CLOUD_SEND),
                "--to",
                MEMO_TO,
                "--subject",
                f"{suite} iteration {args.iteration}",
                "--body-file",
                str(out),
            ],
            check=True,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
