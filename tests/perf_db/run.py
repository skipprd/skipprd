#!/usr/bin/env python3
"""Ingest then skipprd query --plain. Shares skippr-perf helpers; does not import tests.perf.run by path."""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
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
INGEST_WORKSPACE = "perf"
CDC_WORKSPACE = "test"
CLUSTER_TENANT = "default"
OVERLAP_WAIT_SECONDS = 480


def load_perf_db_catalog() -> dict[str, Any]:
    return load_catalog(CATALOG_PATH)


def log(message: str) -> None:
    print(f"[skippr-perf-db] {message}", flush=True)


def compaction_hold_paths(data_dir: Path, workspace: str, pipeline: str) -> list[Path]:
    return [
        data_dir / "failpoint",
        data_dir / "clustered" / CLUSTER_TENANT / workspace / pipeline / "failpoint",
    ]


def arm_compaction_hold(data_dir: Path, workspace: str, pipeline: str) -> list[Path]:
    paths = compaction_hold_paths(data_dir, workspace, pipeline)
    for path in paths:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("hold_before_compaction_sink\n", encoding="utf-8")
    return paths


def disarm_compaction_hold(paths: list[Path]) -> None:
    for path in paths:
        path.unlink(missing_ok=True)


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
    if asserts.get("query_count") is True:
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
    if asserts.get("query_unique"):
        ids = [str(row[0]) for row in result_rows]
        if len(ids) != len(set(ids)):
            raise perf_run.PerfError(f"{scenario_id}: duplicate ids in overlap query")
    expect_name = asserts.get("query_name")
    if expect_name:
        got = str(result_rows[0][0]) if result_rows else ""
        if got != str(expect_name):
            raise perf_run.PerfError(f"{scenario_id}: name {got!r} != {expect_name!r}")
    if "query_count" in asserts and asserts["query_count"] not in {True, False}:
        got = int(float(result_rows[0][0]))
        expect = int(asserts["query_count"])
        if got != expect:
            raise perf_run.PerfError(f"{scenario_id}: count {got} != {expect}")
    if "query_sum" in asserts:
        got = int(float(result_rows[0][0]))
        expect = int(asserts["query_sum"])
        if got != expect:
            raise perf_run.PerfError(f"{scenario_id}: sum {got} != {expect}")
    if asserts.get("stream_limit"):
        expect = stream_limit_rows(scenario)
        if len(result_rows) != expect:
            raise perf_run.PerfError(
                f"{scenario_id}: STREAM returned {len(result_rows)} rows, expected {expect}"
            )


def stream_limit_rows(scenario: dict[str, Any]) -> int:
    sql = str(scenario.get("sql") or "")
    expect = 10
    upper = sql.upper()
    if "LIMIT" in upper:
        try:
            expect = int(upper.rsplit("LIMIT", 1)[1].strip().split()[0])
        except ValueError:
            expect = 10
    return expect


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
    tail_proc = None
    overlap_proc = None
    hold_paths: list[Path] = []
    logs = ""
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
        logs += perf_run.run_skipprd(
            skipprd,
            ["--config", str(config), "discover", "--pipeline", PIPELINE],
            env=env,
            work=work,
            profile="none",
        )
        sync_cmd = [
            str(skipprd),
            *log_args(env),
            "--config",
            str(config),
            "sync",
            "--pipeline",
            PIPELINE,
            "--once",
        ]
        measured_sync = ""
        if scenario.get("wal_tail"):
            hold_paths = arm_compaction_hold(data_dir, INGEST_WORKSPACE, PIPELINE)
            tail_proc = subprocess.Popen(
                sync_cmd,
                env=env,
                cwd=REPO_ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            expect = stream_limit_rows(scenario)
            deadline = time.time() + 90
            while time.time() < deadline:
                try:
                    peek = perf_run.run_skipprd(
                        skipprd,
                        [
                            "--config",
                            str(config),
                            "query",
                            "--plain",
                            "--sql",
                            str(scenario["sql"]),
                        ],
                        env=env,
                        work=work,
                        profile="none",
                    )
                    doc = parse_plain_query(peek)
                    if len(doc.get("rows") or []) == expect:
                        break
                except Exception:
                    pass
                hold_armed = any(path.is_file() for path in hold_paths)
                if tail_proc.poll() is not None and not hold_armed:
                    break
                time.sleep(1)
        else:
            measured_sync = perf_run.run_skipprd(
                skipprd,
                ["--config", str(config), "sync", "--pipeline", PIPELINE, "--once"],
                env=env,
                work=work,
                profile="none",
            )
            logs += measured_sync
        if scenario.get("overlap"):
            hold_paths = arm_compaction_hold(data_dir, INGEST_WORKSPACE, PIPELINE)
            records = [
                {"id": index, "name": f"user_{index}", "amount": index * 10}
                for index in range(rows + 1, (2 * rows) + 1)
            ]
            (source / "events_b.json").write_text(
                "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in records),
                encoding="utf-8",
            )
            rows = 2 * rows
            overlap_cmd = [
                str(skipprd),
                *log_args(env),
                "--config",
                str(config),
                "sync",
                "--pipeline",
                PIPELINE,
                "--once",
            ]
            overlap_proc = subprocess.Popen(
                overlap_cmd,
                env=env,
                cwd=REPO_ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            deadline = time.time() + OVERLAP_WAIT_SECONDS
            while time.time() < deadline:
                try:
                    peek = perf_run.run_skipprd(
                        skipprd,
                        [
                            "--config",
                            str(config),
                            "query",
                            "--plain",
                            "--sql",
                            str(scenario["sql"]),
                        ],
                        env=env,
                        work=work,
                        profile="none",
                    )
                    doc = parse_plain_query(peek)
                    got = int(float((doc.get("rows") or [[0]])[0][0]))
                    if got == rows:
                        break
                except Exception:
                    pass
                if overlap_proc.poll() is not None:
                    break
                time.sleep(1)
        if (scenario.get("asserts") or {}).get("iceberg_parquet") and not scenario.get("wal_tail"):
            parquet_files = list(warehouse.rglob("*.parquet"))
            if not parquet_files:
                raise perf_run.PerfError(
                    f"{scenario_id}: no Iceberg parquet under {warehouse}"
                )
        sql = str(scenario["sql"])
        log(f"skipprd query --sql {sql!r} --plain --warmup")
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
                "--warmup",
            ],
            profile,
            work,
        )
        measured_query = perf_run.run_logged(query_cmd, env=env)
        if scenario.get("wal_tail"):
            disarm_compaction_hold(hold_paths)
            hold_paths = []
            if tail_proc is not None:
                try:
                    out, _ = tail_proc.communicate(timeout=120)
                    measured_sync = out or ""
                    logs += measured_sync
                except Exception:
                    tail_proc.kill()
                tail_proc = None
            parquet_files = list(warehouse.rglob("*.parquet"))
            if (scenario.get("asserts") or {}).get("iceberg_parquet") and not parquet_files:
                raise perf_run.PerfError(
                    f"{scenario_id}: no Iceberg parquet under {warehouse}"
                )
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
        disarm_compaction_hold(hold_paths)
        if overlap_proc is not None:
            try:
                out, _ = overlap_proc.communicate(timeout=120)
                logs += out or ""
            except Exception:
                overlap_proc.kill()
        if tail_proc is not None:
            try:
                tail_proc.communicate(timeout=120)
            except Exception:
                tail_proc.kill()
        if sink == "skipprlake":
            perf_run.stop_ddb()


PG_CDC_PORT = 5435
PG_CDC_DDB_PORT = 18022
PG_CDC_CONTAINER = "skippr-perf-db-pg"
PG_CDC_DDB_CONTAINER = "skippr-perf-db-pg-ddb"
PG_CDC_PIPELINE = "postgres_skipprlake_cdc"
CDC_DDB_TABLE = "skippr-perf-db-cdc"


def docker_available() -> bool:
    for _ in range(5):
        try:
            subprocess.run(
                ["docker", "info"],
                capture_output=True,
                check=True,
                timeout=10,
            )
            return True
        except Exception:
            time.sleep(1)
    return False


def write_cdc_skippr_yml(path: Path, warehouse: Path) -> None:
    uri = f"file://{warehouse}"
    path.write_text(
        f"""skippr:
  workspace: test
  skipprd_el_storage_mode: local
  store:
    type: dynamodb
    name: {CDC_DDB_TABLE}

pipelines:
  {PG_CDC_PIPELINE}:
    auto_approve: yes
    env: test
    type: cdc
    buffer_threshold_bytes: 20000000
    buffer_threshold_seconds: 2
    data_source: data_sources.postgres_cdc
    data_sink: data_sinks.lake
    cdc:
      default:
        business_key_columns: ["id"]

data_sources:
  postgres_cdc:
    Postgres:
      host: 127.0.0.1
      port: {PG_CDC_PORT}
      user: postgres
      password: postgres
      database: skippr_cdc
      tables: ["orders"]
      cdc_mode: snapshot_then_cdc
      cdc_idle_timeout_seconds: 5

data_sinks:
  lake:
    SkipprLake:
      warehouse: {uri}
      catalog_table: {CDC_DDB_TABLE}
      region: us-east-1
      table_namespace: bronze
      object_store:
        type: file
    schema_sink: schema_sinks.lake_schema

schema_sinks:
  lake_schema:
    SkipprLake:
      warehouse: {uri}
      catalog_table: {CDC_DDB_TABLE}
      region: us-east-1
      table_namespace: bronze
      object_store:
        type: file
""",
        encoding="utf-8",
    )


def start_cdc_postgres() -> None:
    subprocess.run(["docker", "rm", "-f", PG_CDC_CONTAINER], capture_output=True, check=False)
    subprocess.run(
        [
            "docker",
            "run",
            "-d",
            "--name",
            PG_CDC_CONTAINER,
            "-e",
            "POSTGRES_PASSWORD=postgres",
            "-e",
            "POSTGRES_DB=skippr_cdc",
            "-p",
            f"{PG_CDC_PORT}:5432",
            "postgres:16-alpine",
            "postgres",
            "-c",
            "wal_level=logical",
        ],
        check=True,
    )
    for _ in range(60):
        ready = subprocess.run(
            [
                "docker",
                "exec",
                PG_CDC_CONTAINER,
                "psql",
                "-U",
                "postgres",
                "-d",
                "skippr_cdc",
                "-c",
                "SELECT 1",
            ],
            capture_output=True,
        )
        if ready.returncode == 0:
            break
        time.sleep(1)
    else:
        raise perf_run.PerfError("postgres CDC container did not accept SQL")
    subprocess.run(
        [
            "docker",
            "exec",
            PG_CDC_CONTAINER,
            "psql",
            "-U",
            "postgres",
            "-d",
            "skippr_cdc",
            "-c",
            "CREATE TABLE IF NOT EXISTS orders (id integer PRIMARY KEY, name text NOT NULL, amount integer NOT NULL DEFAULT 0); INSERT INTO orders (id, name, amount) VALUES (1, 'alpha', 100), (2, 'beta', 50) ON CONFLICT (id) DO NOTHING;",
        ],
        check=True,
    )


def stop_cdc_postgres() -> None:
    subprocess.run(["docker", "rm", "-f", PG_CDC_CONTAINER], capture_output=True, check=False)
    subprocess.run(["docker", "rm", "-f", PG_CDC_DDB_CONTAINER], capture_output=True, check=False)


def pg_sql(sql: str) -> None:
    subprocess.run(
        [
            "docker",
            "exec",
            PG_CDC_CONTAINER,
            "psql",
            "-U",
            "postgres",
            "-d",
            "skippr_cdc",
            "-c",
            sql,
        ],
        check=True,
    )


def run_cdc_once(skipprd: Path, config: Path, env: dict[str, str]) -> str:
    cmd = [
        str(skipprd),
        *log_args(env),
        "--config",
        str(config),
        "sync",
        "--log",
        "--pipeline",
        PG_CDC_PIPELINE,
        "--once",
    ]
    gnu = shutil.which("gtimeout") or shutil.which("timeout")
    if gnu is not None:
        cmd = [gnu, "--signal=KILL", "120", *cmd]
        return perf_run.run_logged(cmd, env=env, timeout=130)
    return perf_run.run_logged(cmd, env=env, timeout=120)


def run_query_after_cdc(
    scenario_id: str,
    scenario: dict[str, Any],
    work: Path,
    profile: str,
) -> dict[str, Any]:
    if not docker_available():
        if os.environ.get("GITHUB_ACTIONS"):
            raise perf_run.PerfError(f"{scenario_id}: Docker required on GitHub")
        log(f"{scenario_id} SKIP (Docker not running)")
        payload = {"scenario": scenario_id, "skipped": True, "reason": "docker-down"}
        (work / "perf-db-report.json").write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        (work / "perf-report.json").write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        return payload
    warehouse = work / "warehouse"
    data_dir = work / "data"
    manifests = work / "manifests"
    warehouse.mkdir(parents=True, exist_ok=True)
    data_dir.mkdir(parents=True, exist_ok=True)
    config = work / "skippr.yml"
    write_cdc_skippr_yml(config, warehouse)
    from e2e_support import dynamodb_local as ddb_local

    overlap_proc = None
    hold_paths: list[Path] = []
    start_cdc_postgres()
    try:
        ddb_local.start(
            port=PG_CDC_DDB_PORT,
            tables=[CDC_DDB_TABLE],
            container=PG_CDC_DDB_CONTAINER,
        )
        perf_run.stage_plugins(config, PG_CDC_PIPELINE, manifests)
        env = perf_run.base_env(config, data_dir, manifests)
        env.update(
            {
                "AWS_ENDPOINT_URL_DYNAMODB": f"http://127.0.0.1:{PG_CDC_DDB_PORT}",
                "AWS_ENDPOINT_URL": f"http://127.0.0.1:{PG_CDC_DDB_PORT}",
                "AWS_ACCESS_KEY_ID": "local",
                "AWS_SECRET_ACCESS_KEY": "local",
                "AWS_REGION": "us-east-1",
                "AWS_DEFAULT_REGION": "us-east-1",
                "AWS_EC2_METADATA_DISABLED": "true",
                "SKIPPRLAKE_TOKEN": "skippr-perf-db-cdc",
            }
        )
        if scenario.get("iceberg_only"):
            env["SKIPPR_QUERY_LAKE_ONLY"] = "1"
        skipprd = perf_run.skipprd_bin()
        logs = run_cdc_once(skipprd, config, env)
        mutation = str(scenario.get("mutation") or "")
        if mutation:
            pg_sql(mutation)
        if scenario.get("overlap"):
            hold_paths = arm_compaction_hold(data_dir, CDC_WORKSPACE, PG_CDC_PIPELINE)
            overlap_cmd = [
                str(skipprd),
                *log_args(env),
                "--config",
                str(config),
                "sync",
                "--log",
                "--pipeline",
                PG_CDC_PIPELINE,
                "--once",
            ]
            overlap_proc = subprocess.Popen(
                overlap_cmd, env=env, cwd=REPO_ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
            )
            expect_name = str((scenario.get("asserts") or {}).get("query_name") or "")
            deadline = time.time() + OVERLAP_WAIT_SECONDS
            while time.time() < deadline:
                try:
                    peek = perf_run.run_skipprd(
                        skipprd,
                        [
                            "--config",
                            str(config),
                            "query",
                            "--plain",
                            "--sql",
                            str(scenario["sql"]),
                        ],
                        env=env,
                        work=work,
                        profile="none",
                    )
                    doc = parse_plain_query(peek)
                    got = str(((doc.get("rows") or [[""]])[0] or [""])[0])
                    if expect_name and got == expect_name:
                        break
                except Exception:
                    pass
                if overlap_proc.poll() is not None:
                    break
                time.sleep(1)
        else:
            logs += run_cdc_once(skipprd, config, env)
        sql = str(scenario["sql"])
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
                "--warmup",
            ],
            profile,
            work,
        )
        measured_query = perf_run.run_logged(query_cmd, env=env)
        logs += measured_query
        disarm_compaction_hold(hold_paths)
        if overlap_proc is not None:
            try:
                out, _ = overlap_proc.communicate(timeout=120)
                logs += out or ""
            except Exception:
                overlap_proc.kill()
        perf_run.convert_flamegraph(work)
        perf_run.require_profile_outputs(work, profile)
        events = parse_perf_events(logs + measured_query)
        required = required_perf_for(load_perf_db_catalog(), scenario)
        missing = [phase for phase in required if not any(ev.get("phase") == phase for ev in events)]
        if missing:
            raise perf_run.PerfError(f"missing PERF phases {missing}; events={events}")
        doc = parse_plain_query(measured_query)
        assert_query_doc(scenario_id, scenario, 0, doc)
        absent = (scenario.get("asserts") or {}).get("query_absent_id")
        if absent is not None:
            ids_doc = parse_plain_query(
                perf_run.run_skipprd(
                    skipprd,
                    [
                        "--config",
                        str(config),
                        "query",
                        "--plain",
                        "--sql",
                        "SELECT id FROM postgres_skipprlake_cdc.postgres_orders",
                    ],
                    env=env,
                    work=work,
                    profile="none",
                )
            )
            ids = {str(row[0]) for row in ids_doc.get("rows") or []}
            if str(absent) in ids:
                raise perf_run.PerfError(f"{scenario_id}: id {absent} still present")
        payload = perf_run.report_payload(scenario_id, 0, 0, events, logs, work)
        payload["kind"] = "query_after_cdc"
        payload["sql"] = sql
        perf_run.write_report(work / "perf-db-report.json", payload)
        (work / "perf-report.json").write_text(
            json.dumps(payload, indent=2) + "\n",
            encoding="utf-8",
        )
        return payload
    finally:
        disarm_compaction_hold(hold_paths)
        if overlap_proc is not None:
            try:
                overlap_proc.communicate(timeout=120)
            except Exception:
                overlap_proc.kill()
        ddb_local.stop(port=PG_CDC_DDB_PORT, container=PG_CDC_DDB_CONTAINER)
        stop_cdc_postgres()


def run_scenario(scenario_id: str, scale: str, work: Path, profile: str) -> dict[str, Any]:
    catalog = load_perf_db_catalog()
    scenario = catalog["scenarios"][scenario_id]
    kind = scenario_kind(scenario)
    if kind == "query_after_ingest":
        return run_query_after_ingest(scenario_id, scenario, scale, work, profile)
    if kind == "query_after_cdc":
        return run_query_after_cdc(scenario_id, scenario, work, profile)
    raise perf_run.PerfError(f"unknown kind {kind}")


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
