#!/usr/bin/env python3
"""Ingest then skipprd query --plain. Shares skippr-perf helpers; does not import tests.perf.run by path."""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
import tempfile
from pathlib import Path
from typing import Any

HARNESS_DIR = Path(__file__).resolve().parent
REPO_ROOT = HARNESS_DIR.parents[1]
PERF_DIR = REPO_ROOT / "tests" / "perf"
sys.path.insert(0, str(PERF_DIR))
sys.path.insert(0, str(REPO_ROOT / "tests"))
sys.path.insert(0, str(REPO_ROOT / ".github" / "scripts"))
sys.path.insert(0, str(HARNESS_DIR))

from catalog import load_catalog, required_perf_for, scenario_kind  # noqa: E402
from fixtures import write_ndjson  # noqa: E402
from parse import parse_perf_events  # noqa: E402
from skipprd_cli import log_args  # noqa: E402

_run_spec = importlib.util.spec_from_file_location("skippr_perf_run", PERF_DIR / "run.py")
if _run_spec is None or _run_spec.loader is None:
    raise ImportError(f"cannot load {PERF_DIR / 'run.py'}")
perf_run = importlib.util.module_from_spec(_run_spec)
sys.modules["skippr_perf_run"] = perf_run
_run_spec.loader.exec_module(perf_run)

PIPELINE = perf_run.PIPELINE
COMMAND_TIMEOUT_SECONDS = perf_run.COMMAND_TIMEOUT_SECONDS
CATALOG_PATH = HARNESS_DIR / "catalog.yaml"


def load_perf_db_catalog() -> dict[str, Any]:
    return load_catalog(CATALOG_PATH)


def log(message: str) -> None:
    print(f"[skippr-perf-db] {message}", flush=True)


def parse_plain_query(captured: str) -> dict[str, Any]:
    for line in reversed(captured.splitlines()):
        stripped = line.strip()
        if stripped.startswith("{") and '"header"' in stripped and '"rows"' in stripped:
            return json.loads(stripped)
    raise perf_run.PerfError("no skipprd query --plain JSON")


def assert_query_doc(
    scenario_id: str,
    scenario: dict[str, Any],
    rows: int,
    doc: dict[str, Any],
) -> None:
    asserts = scenario.get("asserts") or {}
    result_rows = doc.get("rows") or []
    if asserts.get("query_count"):
        if len(result_rows) != 1 or len(result_rows[0]) < 1:
            raise perf_run.PerfError(f"{scenario_id}: query_count expected one cell, got {doc}")
        got = int(float(result_rows[0][0]))
        if got != rows:
            raise perf_run.PerfError(f"{scenario_id}: count {got} != ingest rows {rows}")
    if asserts.get("query_point"):
        if len(result_rows) != 1:
            raise perf_run.PerfError(f"{scenario_id}: query_point expected 1 row, got {len(result_rows)}")
        cell = str(result_rows[0][0])
        if cell not in {"1", "1.0"}:
            raise perf_run.PerfError(f"{scenario_id}: query_point id {cell!r} != 1")
    if asserts.get("query_group"):
        if len(result_rows) < 1:
            raise perf_run.PerfError(f"{scenario_id}: query_group returned no groups")
    if asserts.get("stream_limit"):
        sql = str(scenario.get("sql") or "")
        expect = 10
        upper = sql.upper()
        if "LIMIT" in upper:
            try:
                expect = int(upper.rsplit("LIMIT", 1)[1].strip().split()[0])
            except ValueError:
                expect = 10
        if len(result_rows) != expect:
            raise perf_run.PerfError(
                f"{scenario_id}: STREAM returned {len(result_rows)} rows, expected {expect}"
            )


def run_query_after_ingest(
    scenario_id: str,
    scenario: dict[str, Any],
    scale: str,
    work: Path,
    profile: str,
) -> dict[str, Any]:
    rows = perf_run.row_count(scenario, scale)
    source = work / "source"
    warehouse = work / "warehouse"
    data_dir = work / "data"
    manifests = work / "manifests"
    source.mkdir(parents=True, exist_ok=True)
    warehouse.mkdir(parents=True, exist_ok=True)
    data_dir.mkdir(parents=True, exist_ok=True)
    fixture_kind = str(scenario.get("fixture", "exact_arrow"))
    sink = str(scenario["sink"])
    config = work / "skippr.yml"
    perf_run.write_skippr_yml(
        config,
        kind="ingest",
        source_kind="File",
        source_path=str(source),
        sink=sink,
        warehouse=warehouse,
        flatten=False,
    )
    if sink == "skipprlake":
        perf_run.start_ddb()
    try:
        perf_run.stage_plugins(config, PIPELINE, manifests)
        env = perf_run.base_env(config, data_dir, manifests)
        if sink == "skipprlake":
            env.update(
                {
                    "AWS_ENDPOINT_URL_DYNAMODB": f"http://127.0.0.1:{perf_run.DDB_PORT}",
                    "AWS_ENDPOINT_URL": f"http://127.0.0.1:{perf_run.DDB_PORT}",
                    "AWS_ACCESS_KEY_ID": "local",
                    "AWS_SECRET_ACCESS_KEY": "local",
                    "AWS_REGION": "us-east-1",
                    "AWS_DEFAULT_REGION": "us-east-1",
                    "AWS_EC2_METADATA_DISABLED": "true",
                    "SKIPPRLAKE_TOKEN": "skippr-perf-db",
                }
            )
        if scenario.get("iceberg_only"):
            env["SKIPPR_QUERY_LAKE_ONLY"] = "1"
        skipprd = perf_run.skipprd_bin()
        write_ndjson(source / "events.json", fixture_kind, rows)
        logs = ""
        logs += perf_run.run_skipprd(
            skipprd,
            ["--config", str(config), "discover", "--pipeline", PIPELINE],
            env=env,
            work=work,
            profile="none",
        )
        measured_sync = perf_run.run_skipprd(
            skipprd,
            ["--config", str(config), "sync", "--pipeline", PIPELINE, "--once"],
            env=env,
            work=work,
            profile="none",
        )
        logs += measured_sync
        if (scenario.get("asserts") or {}).get("iceberg_parquet"):
            parquet_files = list(warehouse.rglob("*.parquet"))
            if not parquet_files:
                raise perf_run.PerfError(
                    f"{scenario_id}: no Iceberg parquet under {warehouse}"
                )
        sql = str(scenario["sql"])
        log(f"skipprd query --sql {sql!r} --plain")
        query_cmd = perf_run.time_prefix() + perf_run.wrap_profile(
            [
                str(skipprd),
                *log_args(env),
                "--config",
                str(config),
                "query",
                "--sql",
                sql,
                "--plain",
            ],
            profile,
            work,
        )
        measured_query = perf_run.run_logged(query_cmd, env=env)
        perf_run.convert_flamegraph(work)
        perf_run.require_profile_outputs(work, profile)
        logs += measured_query
        events = parse_perf_events(measured_sync + measured_query)
        required = required_perf_for(load_perf_db_catalog(), scenario)
        missing = [phase for phase in required if not any(ev.get("phase") == phase for ev in events)]
        if missing:
            raise perf_run.PerfError(f"missing PERF phases {missing}; events={events}")
        doc = parse_plain_query(measured_query)
        assert_query_doc(scenario_id, scenario, rows, doc)
        payload = perf_run.report_payload(scenario_id, rows, 0, events, logs, work)
        payload["kind"] = "query_after_ingest"
        payload["sql"] = sql
        payload["sink"] = sink
        query_events = [ev for ev in events if ev.get("phase") == "query_collect"]
        if query_events:
            payload["query_collect_ms"] = int(query_events[-1].get("elapsed_ms") or 0)
            payload["elapsed_ms"] = payload["query_collect_ms"]
        report = work / "perf-db-report.json"
        perf_run.write_report(report, payload)
        (work / "perf-report.json").write_text(
            json.dumps(payload, indent=2) + "\n",
            encoding="utf-8",
        )
        return payload
    finally:
        if sink == "skipprlake":
            perf_run.stop_ddb()


def run_scenario(scenario_id: str, scale: str, work: Path, profile: str) -> dict[str, Any]:
    catalog = load_perf_db_catalog()
    scenario = catalog["scenarios"][scenario_id]
    kind = scenario_kind(scenario)
    if kind != "query_after_ingest":
        raise perf_run.PerfError(f"unknown kind {kind}")
    return run_query_after_ingest(scenario_id, scenario, scale, work, profile)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="skippr-perf-db ingest then query")
    parser.add_argument("--scenario", required=True, help="Scenario id or comma-separated ids")
    parser.add_argument("--scale", choices=("smoke", "ci", "linux"), default="smoke")
    parser.add_argument(
        "--profile",
        choices=("none", "flamegraph", "heaptrack", "perf-stat"),
        default="none",
    )
    parser.add_argument("--work", default="")
    args = parser.parse_args(argv)
    scenarios = load_perf_db_catalog()["scenarios"]
    ids = [part.strip() for part in args.scenario.split(",") if part.strip()]
    unknown = [item for item in ids if item not in scenarios]
    if unknown:
        raise SystemExit(f"unknown scenario(s): {unknown}")
    failures: list[str] = []
    for scenario_id in ids:
        work = Path(args.work) if args.work else Path(tempfile.mkdtemp(prefix=f"skippr-perf-db-{scenario_id}-"))
        work = work / scenario_id if args.work else work
        work.mkdir(parents=True, exist_ok=True)
        log(f"running {scenario_id} work={work}")
        try:
            run_scenario(scenario_id, args.scale, work, args.profile)
        except Exception as err:  # noqa: BLE001 — harness reports then continues comma-ids
            log(f"{scenario_id} FAILED: {err}")
            failures.append(scenario_id)
            payload = {"scenario": scenario_id, "error": str(err)}
            (work / "perf-db-report.json").write_text(
                json.dumps(payload, indent=2) + "\n",
                encoding="utf-8",
            )
            (work / "perf-report.json").write_text(
                json.dumps(payload, indent=2) + "\n",
                encoding="utf-8",
            )
            if len(ids) == 1:
                raise
    if failures:
        raise SystemExit(f"failed: {failures}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except perf_run.PerfError as err:
        print(f"skippr-perf-db failed: {err}", file=sys.stderr)
        raise SystemExit(1)
