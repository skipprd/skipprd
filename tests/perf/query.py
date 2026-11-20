#!/usr/bin/env python3
"""Ingest then skipprd query --plain. Dispatched from run.py by scenario kind."""

from __future__ import annotations

import importlib.util
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Callable

HARNESS_DIR = Path(__file__).resolve().parent
REPO_ROOT = HARNESS_DIR.parents[1]
sys.path.insert(0, str(REPO_ROOT / "tests"))
sys.path.insert(0, str(REPO_ROOT / ".github" / "scripts"))
sys.path.insert(0, str(HARNESS_DIR))

from catalog import load_catalog, required_perf_for  # noqa: E402
from fixtures import write_ndjson  # noqa: E402
from matrix import CDC_CASE_SQL, CDC_PIPELINES, CDC_TABLES  # noqa: E402
from parse import parse_perf_events  # noqa: E402
from skipprd_cli import log_args  # noqa: E402

_run_spec = importlib.util.spec_from_file_location("skippr_perf_run", HARNESS_DIR / "run.py")
if _run_spec is None or _run_spec.loader is None:
    raise ImportError(f"cannot load {HARNESS_DIR / 'run.py'}")
perf_run = importlib.util.module_from_spec(_run_spec)
sys.modules["skippr_perf_run"] = perf_run
_run_spec.loader.exec_module(perf_run)

PIPELINE = perf_run.PIPELINE
COMMAND_TIMEOUT_SECONDS = perf_run.COMMAND_TIMEOUT_SECONDS
INGEST_WORKSPACE = "perf"
CDC_WORKSPACE = "test"
OVERLAP_WAIT_SECONDS = 480
PROFILE_QUERY_REPEAT = 200


def query_plain_args(
    skipprd: Path,
    config: Path,
    env: dict[str, str],
    sql: str,
    profile: str,
) -> list[str]:
    cmd = [
        str(skipprd),
        *log_args(env),
        "--config",
        str(config),
        "query",
        "--sql",
        sql,
        "--plain",
        "--warmup",
    ]
    if profile not in {"", "none"}:
        cmd.extend(["--repeat", str(PROFILE_QUERY_REPEAT)])
    return cmd


def row_count(scenario: dict[str, Any], scale: str) -> int:
    defaults = load_catalog().get("defaults", {})
    if scale == "smoke":
        return int(scenario.get("smoke_rows", defaults.get("smoke_rows", 200)))
    if scale == "linux":
        return int(scenario.get("linux_rows", defaults.get("linux_rows", 2000)))
    return int(scenario.get("rows", defaults.get("rows", 10000)))


def log(message: str) -> None:
    print(f"[skippr-perf] {message}", flush=True)


def arm_compaction_hold(data_dir: Path, workspace: str, pipeline: str) -> Path:
    # Local storage mode pipeline root is `{DATA_DIR}/{workspace}_{pipeline}` (Config::try_data_dir).
    hold = data_dir / f"{workspace}_{pipeline}" / "failpoint"
    hold.parent.mkdir(parents=True, exist_ok=True)
    hold.write_text("hold_before_compaction_sink\n", encoding="utf-8")
    return hold


def disarm_compaction_hold(hold: Path | None) -> None:
    if hold is not None:
        hold.unlink(missing_ok=True)


def require_hold_engaged(scenario_id: str, ingest: subprocess.Popen, hold: Path) -> None:
    """`sync --once` blocks at the hold until disarm; an exit means compaction ran unheld."""
    code = ingest.poll()
    if code is not None:
        raise perf_run.PerfError(
            f"{scenario_id}: compaction hold at {hold} never engaged; ingest exited rc={code}"
        )


def start_held_ingest(sync_cmd: list[str], env: dict[str, str]) -> subprocess.Popen:
    return subprocess.Popen(
        sync_cmd,
        env=env,
        cwd=REPO_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )


def first_cell(doc: dict[str, Any]) -> str:
    rows = doc.get("rows") or [[""]]
    return str((rows[0] or [""])[0])


def peek_until(
    skipprd: Path,
    config: Path,
    env: dict[str, str],
    work: Path,
    sql: str,
    ingest: subprocess.Popen,
    *,
    seconds: int,
    done: Callable[[dict[str, Any]], bool],
    scenario_id: str,
) -> None:
    """Poll `sql` while the held ingest runs; the caller asserts the hold afterwards."""
    deadline = time.time() + seconds
    last_peek = ""
    while time.time() < deadline and ingest.poll() is None:
        try:
            peek = perf_run.run_skipprd(
                skipprd,
                ["--config", str(config), "query", "--plain", "--sql", sql],
                env=env,
                work=work,
                profile="none",
            )
            last_peek = peek[-2000:]
            if done(parse_plain_query(peek)):
                return
        except Exception as err:  # noqa: BLE001 — peeks race ingest startup; the measured query asserts
            last_peek = f"{type(err).__name__}: {err}"
        time.sleep(1)
    code = ingest.poll()
    detail = f"\nlast peek: {last_peek}" if last_peek else ""
    if code is not None:
        raise perf_run.PerfError(
            f"{scenario_id}: follow ingest exited rc={code} before the expected query result{detail}"
        )
    raise perf_run.PerfError(
        f"{scenario_id}: held ingest never reached the expected query result{detail}"
    )


def wait_log_contains(
    log_path: Path, needle: str, ingest: subprocess.Popen, *, seconds: int, scenario_id: str
) -> None:
    deadline = time.time() + seconds
    while time.time() < deadline and ingest.poll() is None:
        text = log_path.read_text(encoding="utf-8") if log_path.is_file() else ""
        if needle in text:
            return
        time.sleep(0.2)
    detail = log_path.read_text(encoding="utf-8") if log_path.is_file() else ""
    raise perf_run.PerfError(
        f"{scenario_id}: follow sync never logged {needle!r}\n{detail[-2000:]}"
    )


def start_logged_ingest(
    sync_cmd: list[str], env: dict[str, str], log_path: Path
) -> subprocess.Popen:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    handle = log_path.open("w", encoding="utf-8")
    return subprocess.Popen(
        sync_cmd,
        env=env,
        cwd=REPO_ROOT,
        stdout=handle,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )


def numeric_query_count(value: object) -> int | None:
    """YAML `1` is int, not bool. `1 in {True, False}` is true in Python — do not use that."""
    if value is None or isinstance(value, bool):
        return None
    return int(value)


def cdc_query_done(scenario: dict[str, Any], doc: dict[str, Any]) -> bool:
    asserts = scenario.get("asserts") or {}
    rows = doc.get("rows") or []
    if not rows:
        return False
    expect_count = numeric_query_count(asserts.get("query_count"))
    if expect_count is not None:
        return str(rows[0][0]) == str(expect_count)
    if "query_sum" in asserts:
        return int(float(rows[0][0])) == int(asserts["query_sum"])
    expect_name = asserts.get("query_name")
    if expect_name:
        header = [str(col) for col in (doc.get("header") or [])]
        name_idx = next(
            (i for i, col in enumerate(header) if col in {"name", "display_name"}),
            0,
        )
        return str(rows[0][name_idx]) == str(expect_name)
    return True


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
    header = [str(col) for col in (doc.get("header") or [])]
    expect_name = asserts.get("query_name")
    if expect_name:
        if len(result_rows) != 1:
            raise perf_run.PerfError(
                f"{scenario_id}: query_name expected 1 row, got {len(result_rows)}: {result_rows}"
            )
        name_idx = next(
            (i for i, col in enumerate(header) if col in {"name", "display_name"}),
            0,
        )
        got = str(result_rows[0][name_idx])
        if got != str(expect_name):
            raise perf_run.PerfError(f"{scenario_id}: name {got!r} != {expect_name!r}")
    has = asserts.get("query_has") or []
    missing = asserts.get("query_missing") or []
    if isinstance(has, str):
        has = [has]
    if isinstance(missing, str):
        missing = [missing]
    for col in has:
        if col not in header:
            raise perf_run.PerfError(
                f"{scenario_id}: query header missing {col!r}; header={header}"
            )
    for col in missing:
        if col in header:
            raise perf_run.PerfError(
                f"{scenario_id}: query header still has {col!r}; header={header}"
            )
    expect_count = numeric_query_count(asserts.get("query_count"))
    if expect_count is not None:
        got = int(float(result_rows[0][0]))
        if got != expect_count:
            raise perf_run.PerfError(f"{scenario_id}: count {got} != {expect_count}")
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


def assert_wal_pin(scenario_id: str, scenario: dict[str, Any], events: list[dict[str, Any]]) -> None:
    """`drained`: the measured query pinned no WAL (`W \\ E` empty). `live`: it read held WAL."""
    expect = (scenario.get("asserts") or {}).get("wal_pin")
    if expect is None:
        return
    if expect not in {"drained", "live"}:
        raise perf_run.PerfError(f"{scenario_id}: unknown wal_pin {expect!r}")
    pins = [ev for ev in events if ev.get("phase") == "query_pin"]
    if not pins:
        raise perf_run.PerfError(f"{scenario_id}: no query_pin PERF event")
    slices = int(pins[-1].get("wal_slices") or 0)
    if expect == "drained" and slices != 0:
        raise perf_run.PerfError(f"{scenario_id}: wal_pin drained but query pinned {slices} WAL slices")
    if expect == "live" and slices == 0:
        raise perf_run.PerfError(f"{scenario_id}: wal_pin live but query pinned no WAL slices")


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
    rows = row_count(scenario, scale)
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
        sql=perf_run.transform_sql_of(scenario) or False,
        compute=perf_run.transform_compute_of(scenario) or False,
    )
    perf_run.start_ddb()
    tail_proc = None
    overlap_proc = None
    hold: Path | None = None
    logs = ""
    try:
        perf_run.stage_plugins(config, PIPELINE, manifests)
        env = perf_run.base_env(config, data_dir, manifests)
        env.update(perf_run.ddb_local_env(perf_run.DDB_PORT, "skippr-perf-db"))
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
        sql = str(scenario["sql"])
        measured_sync = ""
        original_rows = rows
        if scenario.get("wal_tail"):
            hold = arm_compaction_hold(data_dir, INGEST_WORKSPACE, PIPELINE)
            tail_proc = start_held_ingest(sync_cmd, env)
            if scenario.get("alter"):
                deadline = time.time() + 90
                while time.time() < deadline and tail_proc.poll() is None:
                    if perf_run.leftover_segs(data_dir) > 0:
                        break
                    time.sleep(0.2)
                require_hold_engaged(scenario_id, tail_proc, hold)
                logs += perf_run.stop_skipprd(tail_proc)
                tail_proc = None
                perf_run.write_migration_sql(config, str(scenario["alter"]))
                logs += perf_run.run_skipprd(
                    skipprd,
                    ["--config", str(config), "migrate", "--pipeline", PIPELINE],
                    env=env,
                    work=work,
                    profile="none",
                )
                if perf_run.leftover_segs(data_dir) == 0:
                    raise perf_run.PerfError(
                        f"{scenario_id}: stream migrate expected leftover WAL"
                    )
                if scenario.get("continue_ingest"):
                    (source / "events.json").unlink(missing_ok=True)
                    write_ndjson(
                        source / "events_b.json",
                        fixture_kind,
                        original_rows,
                        start=original_rows + 1,
                    )
                    tail_proc = start_held_ingest(sync_cmd, env)
                    expect = stream_limit_rows(scenario)
                    peek_until(
                        skipprd,
                        config,
                        env,
                        work,
                        sql,
                        tail_proc,
                        seconds=90,
                        done=lambda doc: len(doc.get("rows") or []) == expect,
                        scenario_id=scenario_id,
                    )
                    require_hold_engaged(scenario_id, tail_proc, hold)
            else:
                expect = stream_limit_rows(scenario)
                peek_until(
                    skipprd,
                    config,
                    env,
                    work,
                    sql,
                    tail_proc,
                    seconds=90,
                    done=lambda doc: len(doc.get("rows") or []) == expect,
                    scenario_id=scenario_id,
                )
                require_hold_engaged(scenario_id, tail_proc, hold)
        else:
            measured_sync = perf_run.run_skipprd(
                skipprd,
                ["--config", str(config), "sync", "--pipeline", PIPELINE, "--once"],
                env=env,
                work=work,
                profile="none",
            )
            logs += measured_sync
            if scenario.get("alter"):
                perf_run.write_migration_sql(config, str(scenario["alter"]))
                logs += perf_run.run_skipprd(
                    skipprd,
                    ["--config", str(config), "migrate", "--pipeline", PIPELINE],
                    env=env,
                    work=work,
                    profile="none",
                )
            if scenario.get("continue_ingest"):
                (source / "events.json").unlink(missing_ok=True)
                write_ndjson(
                    source / "events_b.json",
                    fixture_kind,
                    original_rows,
                    start=original_rows + 1,
                )
                logs += perf_run.run_skipprd(
                    skipprd,
                    ["--config", str(config), "sync", "--pipeline", PIPELINE, "--once"],
                    env=env,
                    work=work,
                    profile="none",
                )
                rows = original_rows * 2
        if scenario.get("overlap"):
            hold = arm_compaction_hold(data_dir, INGEST_WORKSPACE, PIPELINE)
            records = [
                {"id": index, "name": f"user_{index}", "amount": index * 10}
                for index in range(rows + 1, (2 * rows) + 1)
            ]
            (source / "events_b.json").write_text(
                "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in records),
                encoding="utf-8",
            )
            (source / "events.json").unlink(missing_ok=True)
            rows = 2 * rows
            overlap_proc = start_held_ingest(sync_cmd, env)
            peek_until(
                skipprd,
                config,
                env,
                work,
                sql,
                overlap_proc,
                seconds=OVERLAP_WAIT_SECONDS,
                done=lambda doc: first_cell(doc) == str(rows),
                scenario_id=scenario_id,
            )
            require_hold_engaged(scenario_id, overlap_proc, hold)
        if (scenario.get("asserts") or {}).get("iceberg_parquet") and not scenario.get("wal_tail"):
            parquet_files = list(warehouse.rglob("*.parquet"))
            if not parquet_files:
                raise perf_run.PerfError(
                    f"{scenario_id}: no Iceberg parquet under {warehouse}"
                )
        log(f"skipprd query --sql {sql!r} --plain --warmup")
        query_cmd = perf_run.time_prefix() + perf_run.wrap_profile(
            query_plain_args(skipprd, config, env, sql, profile),
            profile,
            work,
        )
        measured_query = perf_run.run_logged(query_cmd, env=env)
        if scenario.get("wal_tail"):
            disarm_compaction_hold(hold)
            hold = None
            if tail_proc is not None:
                try:
                    out, _ = tail_proc.communicate(timeout=120)
                    measured_sync = out or ""
                    logs += measured_sync
                except Exception:
                    tail_proc.kill()
                tail_proc = None
            elif scenario.get("alter"):
                measured_sync = perf_run.run_skipprd(
                    skipprd,
                    ["--config", str(config), "sync", "--pipeline", PIPELINE, "--once"],
                    env=env,
                    work=work,
                    profile="none",
                )
                logs += measured_sync
            parquet_files = list(warehouse.rglob("*.parquet"))
            if (scenario.get("asserts") or {}).get("iceberg_parquet") and not parquet_files:
                raise perf_run.PerfError(
                    f"{scenario_id}: no Iceberg parquet under {warehouse}"
                )
        perf_run.convert_flamegraph(work)
        perf_run.require_profile_outputs(work, profile)
        logs += measured_query
        events = parse_perf_events(measured_sync + measured_query)
        required = required_perf_for(load_catalog(), scenario)
        missing = [phase for phase in required if not any(ev.get("phase") == phase for ev in events)]
        if missing:
            raise perf_run.PerfError(f"missing PERF phases {missing}; events={events}")
        doc = parse_plain_query(measured_query)
        assert_query_doc(scenario_id, scenario, rows, doc)
        assert_wal_pin(scenario_id, scenario, parse_perf_events(measured_query))
        if scenario.get("continue_ingest"):
            count_out = perf_run.run_skipprd(
                skipprd,
                [
                    "--config",
                    str(config),
                    "query",
                    "--plain",
                    "--sql",
                    "SELECT count(*) AS n FROM perf_cell.perf_cell",
                ],
                env=env,
                work=work,
                profile="none",
            )
            got = int(float((parse_plain_query(count_out).get("rows") or [[0]])[0][0]))
            expect = original_rows * 2
            if got != expect:
                raise perf_run.PerfError(
                    f"{scenario_id}: continue ingest count {got} != {expect}"
                )
        payload = perf_run.report_payload(scenario_id, rows, 0, events, logs, work)
        payload["kind"] = "query_after_ingest"
        payload["sql"] = sql
        payload["sink"] = sink
        query_events = [ev for ev in events if ev.get("phase") == "query_collect"]
        if query_events:
            payload["query_collect_ms"] = int(query_events[-1].get("elapsed_ms") or 0)
            payload["elapsed_ms"] = payload["query_collect_ms"]
        perf_run.write_report(work / "perf-report.json", payload)
        return payload
    finally:
        disarm_compaction_hold(hold)
        for proc in (overlap_proc, tail_proc):
            if proc is not None:
                try:
                    proc.communicate(timeout=120)
                except Exception:
                    proc.kill()
        perf_run.stop_ddb()


PG_CDC_PORT = 5435
PG_CDC_DDB_PORT = 18022
PG_CDC_CONTAINER = "skippr-perf-db-pg"
PG_CDC_DDB_CONTAINER = "skippr-perf-db-pg-ddb"
PG_CDC_PIPELINE = "postgres_skipprlake_cdc"
CDC_DDB_TABLE = "skippr-perf-db-cdc"
MYSQL_CDC_PORT = 3308
MYSQL_CDC_DDB_PORT = 18023
MYSQL_CDC_CONTAINER = "skippr-perf-db-mysql"
MYSQL_CDC_DDB_CONTAINER = "skippr-perf-db-mysql-ddb"
MYSQL_CDC_TABLE = "skippr-perf-db-mysql-cdc"
DDB_CDC_PORT = 18024
DDB_CDC_CONTAINER = "skippr-perf-db-ddb"
DDB_CDC_SOURCE_TABLE = "skippr_dynamo_cdc"
DDB_CDC_CATALOG_TABLE = "skippr-perf-db-ddb-cdc"


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


DDB_EVENT_TS_DAY_A = 1_700_000_000
DDB_EVENT_TS_DAY_B = 1_700_086_400
DDB_EVENT_TS_UPDATE = 1_700_172_800
DDB_EVENT_TS_GAMMA = 1_700_086_400


def _ddb_event_ts_attr(value: int) -> dict[str, dict[str, str]]:
    return {"event_ts": {"N": str(value)}}


def write_cdc_skippr_yml(
    path: Path,
    warehouse: Path,
    source: str = "pg",
    *,
    partition_day: bool = False,
) -> None:
    uri = f"file://{warehouse}"
    pipeline = CDC_PIPELINES[source]
    if source == "pg":
        catalog_table = CDC_DDB_TABLE
        data_source = f"""  postgres_cdc:
    Postgres:
      host: 127.0.0.1
      port: {PG_CDC_PORT}
      user: postgres
      password: postgres
      database: skippr_cdc
      tables: ["orders"]
      cdc_mode: snapshot_then_cdc
      cdc_idle_timeout_seconds: 5"""
        source_ref = "data_sources.postgres_cdc"
        keys = '["id"]'
        extra_transform = ""
    elif source == "mysql":
        catalog_table = MYSQL_CDC_TABLE
        data_source = f"""  mysql_cdc:
    Mysql:
      connection_string: mysql://root:mysql@127.0.0.1:{MYSQL_CDC_PORT}/skippr_cdc
      tables: ["orders"]
      cdc_mode: snapshot_then_cdc
      server_id: 4244
      cdc_idle_timeout_seconds: 5"""
        source_ref = "data_sources.mysql_cdc"
        keys = '["id"]'
        extra_transform = ""
    elif source == "ddb":
        catalog_table = DDB_CDC_CATALOG_TABLE
        data_source = f"""  dynamodb_cdc:
    Dynamodb:
      table_name: {DDB_CDC_SOURCE_TABLE}
      region: us-east-1
      endpoint_url: http://127.0.0.1:{DDB_CDC_PORT}
      cdc_mode: snapshot_then_cdc"""
        source_ref = "data_sources.dynamodb_cdc"
        keys = '["id", "rmc"]'
        extra_transform = ""
        if partition_day:
            extra_transform = """
    transform:
      batch_time_fields: event_ts
      batch_time_unit: day"""
    else:
        raise perf_run.PerfError(f"unknown cdc_source {source!r}")
    path.write_text(
        f"""skippr:
  workspace: test
  skipprd_el_storage_mode: local

pipelines:
  {pipeline}:
    auto_approve: yes
    env: test
    type: cdc
    buffer_threshold_bytes: 20000000
    buffer_threshold_seconds: 2
    data_source: {source_ref}
    data_sink: data_sinks.lake
    cdc:
      default:
        business_key_columns: {keys}{extra_transform}

data_sources:
{data_source}

data_sinks:
  lake:
    SkipprLake:
      warehouse: {uri}
      catalog_table: {catalog_table}
      region: us-east-1
      table_namespace: bronze
      object_store:
        type: file
""",
        encoding="utf-8",
    )


def _cdc_postgres_failed(reason: str) -> perf_run.PerfError:
    logs = subprocess.run(
        ["docker", "logs", "--tail", "80", PG_CDC_CONTAINER],
        capture_output=True,
        text=True,
    )
    detail = f"{logs.stdout}{logs.stderr}".strip()
    return perf_run.PerfError(f"postgres CDC container {reason}\n{detail}")


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
        state = subprocess.run(
            [
                "docker",
                "inspect",
                "-f",
                "{{.State.Running}} {{.State.ExitCode}}",
                PG_CDC_CONTAINER,
            ],
            capture_output=True,
            text=True,
        )
        if state.returncode == 0 and state.stdout.strip().startswith("false"):
            raise _cdc_postgres_failed("exited before accepting SQL")
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
        raise _cdc_postgres_failed("did not accept SQL")
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
            "CREATE TABLE IF NOT EXISTS orders (id integer PRIMARY KEY, name text NOT NULL, amount integer NOT NULL DEFAULT 0); ALTER TABLE orders REPLICA IDENTITY FULL; INSERT INTO orders (id, name, amount) VALUES (1, 'alpha', 100), (2, 'beta', 50) ON CONFLICT (id) DO NOTHING;",
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


def _cdc_mysql_failed(reason: str) -> perf_run.PerfError:
    logs = subprocess.run(
        ["docker", "logs", "--tail", "80", MYSQL_CDC_CONTAINER],
        capture_output=True,
        text=True,
    )
    detail = f"{logs.stdout}{logs.stderr}".strip()
    return perf_run.PerfError(f"mysql CDC container {reason}\n{detail}")


def _mysql_cli(*args: str, check: bool = False) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        [
            "docker",
            "exec",
            MYSQL_CDC_CONTAINER,
            "mysql",
            "-h",
            "127.0.0.1",
            "-uroot",
            "-pmysql",
            *args,
        ],
        capture_output=True,
        check=check,
    )


def start_cdc_mysql() -> None:
    subprocess.run(["docker", "rm", "-f", MYSQL_CDC_CONTAINER], capture_output=True, check=False)
    subprocess.run(
        [
            "docker",
            "run",
            "-d",
            "--name",
            MYSQL_CDC_CONTAINER,
            "-e",
            "MYSQL_ROOT_PASSWORD=mysql",
            "-e",
            "MYSQL_DATABASE=skippr_cdc",
            "-p",
            f"{MYSQL_CDC_PORT}:3306",
            "mysql:8.0",
            "--server-id=1",
            "--log-bin=binlog",
            "--binlog-format=ROW",
            "--binlog-row-image=FULL",
        ],
        check=True,
    )
    create_sql = (
        "CREATE TABLE IF NOT EXISTS orders (id INT PRIMARY KEY, name VARCHAR(64) NOT NULL, "
        "amount INT NOT NULL DEFAULT 0); INSERT IGNORE INTO orders (id, name, amount) VALUES "
        "(1, 'alpha', 100), (2, 'beta', 50);"
    )
    for _ in range(90):
        ready = _mysql_cli("-e", "SELECT 1")
        if ready.returncode != 0:
            time.sleep(1)
            continue
        created = _mysql_cli("skippr_cdc", "-e", create_sql)
        if created.returncode == 0:
            break
        time.sleep(1)
    else:
        raise _cdc_mysql_failed("did not accept SQL")


def stop_cdc_mysql() -> None:
    subprocess.run(["docker", "rm", "-f", MYSQL_CDC_CONTAINER], capture_output=True, check=False)
    subprocess.run(["docker", "rm", "-f", MYSQL_CDC_DDB_CONTAINER], capture_output=True, check=False)


def mysql_sql(sql: str) -> None:
    _mysql_cli("skippr_cdc", "-e", sql, check=True)


def ddb_cdc_client():
    import boto3
    from botocore.config import Config as BotoConfig

    return boto3.client(
        "dynamodb",
        endpoint_url=f"http://127.0.0.1:{DDB_CDC_PORT}",
        region_name="us-east-1",
        aws_access_key_id="local",
        aws_secret_access_key="local",
        config=BotoConfig(retries={"max_attempts": 8}),
    )


def start_cdc_ddb(*, partition_day: bool = False) -> None:
    from botocore.exceptions import ClientError

    subprocess.run(["docker", "rm", "-f", DDB_CDC_CONTAINER], capture_output=True, check=False)
    subprocess.run(
        [
            "docker",
            "run",
            "-d",
            "--name",
            DDB_CDC_CONTAINER,
            "-p",
            f"{DDB_CDC_PORT}:8000",
            "amazon/dynamodb-local",
            "-jar",
            "DynamoDBLocal.jar",
            "-sharedDb",
            "-inMemory",
        ],
        check=True,
    )
    client = ddb_cdc_client()
    deadline = time.time() + 30
    while time.time() < deadline:
        try:
            client.list_tables()
            break
        except Exception:
            time.sleep(0.5)
    else:
        raise perf_run.PerfError("dynamodb CDC Local did not become ready")
    for name, stream in (
        (DDB_CDC_CATALOG_TABLE, False),
        (DDB_CDC_SOURCE_TABLE, True),
    ):
        try:
            client.delete_table(TableName=name)
            client.get_waiter("table_not_exists").wait(TableName=name)
        except ClientError:
            pass
        kwargs: dict[str, Any] = {
            "TableName": name,
            "BillingMode": "PAY_PER_REQUEST",
        }
        if stream:
            kwargs["AttributeDefinitions"] = [
                {"AttributeName": "id", "AttributeType": "S"},
                {"AttributeName": "rmc", "AttributeType": "S"},
            ]
            kwargs["KeySchema"] = [
                {"AttributeName": "id", "KeyType": "HASH"},
                {"AttributeName": "rmc", "KeyType": "RANGE"},
            ]
            kwargs["StreamSpecification"] = {
                "StreamEnabled": True,
                "StreamViewType": "NEW_AND_OLD_IMAGES",
            }
        else:
            kwargs["AttributeDefinitions"] = [
                {"AttributeName": "PK", "AttributeType": "S"},
                {"AttributeName": "SK", "AttributeType": "S"},
            ]
            kwargs["KeySchema"] = [
                {"AttributeName": "PK", "KeyType": "HASH"},
                {"AttributeName": "SK", "KeyType": "RANGE"},
            ]
        client.create_table(**kwargs)
        client.get_waiter("table_exists").wait(TableName=name)
    item_one: dict[str, Any] = {
        "id": {"S": "1"},
        "rmc": {"S": "eu"},
        "name": {"S": "alpha"},
        "amount": {"N": "100"},
    }
    item_two: dict[str, Any] = {
        "id": {"S": "2"},
        "rmc": {"S": "us"},
        "name": {"S": "beta"},
        "amount": {"N": "50"},
    }
    if partition_day:
        item_one.update(_ddb_event_ts_attr(DDB_EVENT_TS_DAY_A))
        item_two.update(_ddb_event_ts_attr(DDB_EVENT_TS_DAY_B))
    client.put_item(TableName=DDB_CDC_SOURCE_TABLE, Item=item_one)
    client.put_item(TableName=DDB_CDC_SOURCE_TABLE, Item=item_two)


def stop_cdc_ddb() -> None:
    subprocess.run(["docker", "rm", "-f", DDB_CDC_CONTAINER], capture_output=True, check=False)


def apply_cdc_case(source: str, case: str, *, partition_day: bool = False) -> None:
    if source == "pg":
        pg_sql(CDC_CASE_SQL[case])
        return
    if source == "mysql":
        mysql_sql(CDC_CASE_SQL[case])
        return
    if source != "ddb":
        raise perf_run.PerfError(f"unknown cdc_source {source!r}")
    client = ddb_cdc_client()

    def item(base: dict[str, Any], ts: int | None = None) -> dict[str, Any]:
        out = dict(base)
        if partition_day and ts is not None:
            out.update(_ddb_event_ts_attr(ts))
        return out

    if case in {"count", "iceberg-only"}:
        client.put_item(
            TableName=DDB_CDC_SOURCE_TABLE,
            Item=item(
                {
                    "id": {"S": "3"},
                    "rmc": {"S": "uk"},
                    "name": {"S": "gamma"},
                    "amount": {"N": "30"},
                },
                DDB_EVENT_TS_GAMMA,
            ),
        )
        return
    if case == "update":
        client.put_item(
            TableName=DDB_CDC_SOURCE_TABLE,
            Item=item(
                {
                    "id": {"S": "1"},
                    "rmc": {"S": "eu"},
                    "name": {"S": "alpha-prime"},
                    "amount": {"N": "100"},
                },
                DDB_EVENT_TS_UPDATE,
            ),
        )
        return
    if case == "delete":
        client.delete_item(
            TableName=DDB_CDC_SOURCE_TABLE,
            Key={"id": {"S": "2"}, "rmc": {"S": "us"}},
        )
        return
    if case == "reinsert":
        client.delete_item(
            TableName=DDB_CDC_SOURCE_TABLE,
            Key={"id": {"S": "2"}, "rmc": {"S": "us"}},
        )
        client.put_item(
            TableName=DDB_CDC_SOURCE_TABLE,
            Item=item(
                {
                    "id": {"S": "2"},
                    "rmc": {"S": "us"},
                    "name": {"S": "beta-new"},
                    "amount": {"N": "50"},
                },
                DDB_EVENT_TS_DAY_B,
            ),
        )
        return
    if case == "sum":
        client.put_item(
            TableName=DDB_CDC_SOURCE_TABLE,
            Item=item(
                {
                    "id": {"S": "3"},
                    "rmc": {"S": "uk"},
                    "name": {"S": "gamma"},
                    "amount": {"N": "30"},
                },
                DDB_EVENT_TS_GAMMA,
            ),
        )
        client.put_item(
            TableName=DDB_CDC_SOURCE_TABLE,
            Item=item(
                {
                    "id": {"S": "1"},
                    "rmc": {"S": "eu"},
                    "name": {"S": "alpha"},
                    "amount": {"N": "120"},
                },
                DDB_EVENT_TS_UPDATE,
            ),
        )
        return
    if case == "overlap":
        client.put_item(
            TableName=DDB_CDC_SOURCE_TABLE,
            Item=item(
                {
                    "id": {"S": "1"},
                    "rmc": {"S": "eu"},
                    "name": {"S": "alpha-wal"},
                    "amount": {"N": "100"},
                },
                DDB_EVENT_TS_UPDATE,
            ),
        )
        return
    raise perf_run.PerfError(f"unknown cdc_case {case!r}")


def cdc_sync_cmd(
    skipprd: Path,
    config: Path,
    env: dict[str, str],
    pipeline: str = PG_CDC_PIPELINE,
    *,
    once: bool = True,
) -> list[str]:
    cmd = [
        str(skipprd),
        *log_args(env),
        "--config",
        str(config),
        "sync",
        "--log",
        "--pipeline",
        pipeline,
    ]
    if once:
        cmd.append("--once")
    return cmd


def with_sync_perf(captured: str, wall_ms: int, rows: int = 0) -> str:
    """CDC `sync --once` is KILL-wrapped; skipprd may never emit phase sync."""
    if any(ev.get("phase") == "sync" for ev in parse_perf_events(captured)):
        return captured
    return (
        captured
        + f'\nPERF_EVENT {{"phase":"sync","elapsed_ms":{int(wall_ms)},"rows":{int(rows)},"bytes":0}}\n'
    )


def run_cdc_once(
    skipprd: Path, config: Path, env: dict[str, str], pipeline: str = PG_CDC_PIPELINE
) -> str:
    cmd = cdc_sync_cmd(skipprd, config, env, pipeline)
    gnu = shutil.which("gtimeout") or shutil.which("timeout")
    started = time.perf_counter()
    if gnu is not None:
        cmd = [gnu, "--signal=KILL", "120", *cmd]
        captured = perf_run.run_logged(cmd, env=env, timeout=130)
    else:
        captured = perf_run.run_logged(cmd, env=env, timeout=120)
    wall_ms = int((time.perf_counter() - started) * 1000)
    return with_sync_perf(captured, wall_ms)


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
        (work / "perf-report.json").write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        return payload
    source = str(scenario.get("cdc_source") or "pg")
    pipeline = CDC_PIPELINES[source]
    table = CDC_TABLES[source]
    warehouse = work / "warehouse"
    data_dir = work / "data"
    manifests = work / "manifests"
    warehouse.mkdir(parents=True, exist_ok=True)
    data_dir.mkdir(parents=True, exist_ok=True)
    config = work / "skippr.yml"
    partition_day = bool(scenario.get("cdc_partition_day"))
    write_cdc_skippr_yml(config, warehouse, source, partition_day=partition_day)
    from e2e_support import dynamodb_local as ddb_local

    overlap_proc = None
    hold: Path | None = None
    catalog_port = PG_CDC_DDB_PORT
    catalog_container = PG_CDC_DDB_CONTAINER
    catalog_tables = [CDC_DDB_TABLE]
    try:
        if source == "pg":
            start_cdc_postgres()
        elif source == "mysql":
            start_cdc_mysql()
            catalog_port = MYSQL_CDC_DDB_PORT
            catalog_container = MYSQL_CDC_DDB_CONTAINER
            catalog_tables = [MYSQL_CDC_TABLE]
        elif source == "ddb":
            start_cdc_ddb(partition_day=partition_day)
            catalog_port = DDB_CDC_PORT
            catalog_container = DDB_CDC_CONTAINER
            catalog_tables = [DDB_CDC_CATALOG_TABLE]
        else:
            raise perf_run.PerfError(f"unknown cdc_source {source!r}")
        if source != "ddb":
            ddb_local.start(
                port=catalog_port,
                tables=catalog_tables,
                container=catalog_container,
            )
        perf_run.stage_plugins(config, pipeline, manifests)
        env = perf_run.base_env(config, data_dir, manifests)
        env.update(perf_run.ddb_local_env(catalog_port, catalog_tables[0]))
        skipprd = perf_run.skipprd_bin()
        sql = str(scenario["sql"])
        logs = run_cdc_once(skipprd, config, env, pipeline)
        if scenario.get("alter") or scenario.get("continue_ingest"):
            if not scenario.get("alter"):
                raise perf_run.PerfError(
                    f"{scenario_id}: continue_ingest on CDC requires alter"
                )
        if scenario.get("alter"):
            perf_run.write_migration_sql(config, str(scenario["alter"]), table)
            logs += perf_run.run_skipprd(
                skipprd,
                ["--config", str(config), "migrate", "--pipeline", pipeline],
                env=env,
                work=work,
                profile="none",
            )
        case = str(scenario.get("cdc_case") or "")
        follow_log = work / "cdc-follow.log"
        if source == "ddb" and case:
            if scenario.get("overlap"):
                hold = arm_compaction_hold(data_dir, CDC_WORKSPACE, pipeline)
            follow_env = dict(env)
            follow_env["SKIPPR_RUNTIME_LOG_LEVEL"] = "info"
            overlap_proc = start_logged_ingest(
                cdc_sync_cmd(skipprd, config, env, pipeline, once=False),
                follow_env,
                follow_log,
            )
            wait_log_contains(
                follow_log,
                "Syncing pipeline",
                overlap_proc,
                seconds=60,
                scenario_id=scenario_id,
            )
            if source == "ddb":
                wait_log_contains(
                    follow_log,
                    "polling shard",
                    overlap_proc,
                    seconds=60,
                    scenario_id=scenario_id,
                )
            apply_cdc_case(source, case, partition_day=partition_day)
            peek_until(
                skipprd,
                config,
                env,
                work,
                sql,
                overlap_proc,
                seconds=OVERLAP_WAIT_SECONDS,
                done=lambda doc, spec=scenario: cdc_query_done(spec, doc),
                scenario_id=scenario_id,
            )
            if hold is not None:
                require_hold_engaged(scenario_id, overlap_proc, hold)
            elif overlap_proc is not None:
                overlap_proc.kill()
                try:
                    overlap_proc.wait(timeout=30)
                except Exception:
                    pass
                handle = overlap_proc.stdout
                if handle is not None and hasattr(handle, "close"):
                    try:
                        handle.close()
                    except Exception:
                        pass
                if follow_log.is_file():
                    logs += follow_log.read_text(encoding="utf-8")
                overlap_proc = None
                logs += run_cdc_once(skipprd, config, env, pipeline)
        else:
            if case:
                apply_cdc_case(source, case, partition_day=partition_day)
            if scenario.get("overlap"):
                hold = arm_compaction_hold(data_dir, CDC_WORKSPACE, pipeline)
                overlap_proc = start_held_ingest(
                    cdc_sync_cmd(skipprd, config, env, pipeline), env
                )
                expect_name = str((scenario.get("asserts") or {}).get("query_name") or "")
                peek_until(
                    skipprd,
                    config,
                    env,
                    work,
                    sql,
                    overlap_proc,
                    seconds=OVERLAP_WAIT_SECONDS,
                    done=lambda doc: first_cell(doc) == expect_name,
                    scenario_id=scenario_id,
                )
                require_hold_engaged(scenario_id, overlap_proc, hold)
            else:
                logs += run_cdc_once(skipprd, config, env, pipeline)
        query_cmd = perf_run.time_prefix() + perf_run.wrap_profile(
            query_plain_args(skipprd, config, env, sql, profile),
            profile,
            work,
        )
        measured_query = perf_run.run_logged(query_cmd, env=env)
        logs += measured_query
        disarm_compaction_hold(hold)
        hold = None
        if overlap_proc is not None:
            overlap_proc.kill()
            try:
                overlap_proc.wait(timeout=30)
            except Exception:
                pass
            handle = overlap_proc.stdout
            if handle is not None and hasattr(handle, "close"):
                try:
                    handle.close()
                except Exception:
                    pass
            if follow_log.is_file():
                logs += follow_log.read_text(encoding="utf-8")
            overlap_proc = None
        perf_run.convert_flamegraph(work)
        perf_run.require_profile_outputs(work, profile)
        events = parse_perf_events(logs)
        required = required_perf_for(load_catalog(), scenario)
        missing = [phase for phase in required if not any(ev.get("phase") == phase for ev in events)]
        if missing:
            raise perf_run.PerfError(f"missing PERF phases {missing}; events={events}")
        doc = parse_plain_query(measured_query)
        assert_query_doc(scenario_id, scenario, 0, doc)
        assert_wal_pin(scenario_id, scenario, parse_perf_events(measured_query))
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
                        f"SELECT id FROM {table}",
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
        perf_run.write_report(work / "perf-report.json", payload)
        return payload
    finally:
        disarm_compaction_hold(hold)
        if overlap_proc is not None:
            try:
                overlap_proc.communicate(timeout=120)
            except Exception:
                overlap_proc.kill()
        if source == "pg":
            ddb_local.stop(port=PG_CDC_DDB_PORT, container=PG_CDC_DDB_CONTAINER)
            stop_cdc_postgres()
        elif source == "mysql":
            ddb_local.stop(port=MYSQL_CDC_DDB_PORT, container=MYSQL_CDC_DDB_CONTAINER)
            stop_cdc_mysql()
        else:
            stop_cdc_ddb()
