#!/usr/bin/env python3
"""Product e2e correctness + performance matrix (release skipprd + local plugins)."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

HARNESS_DIR = Path(__file__).resolve().parent
REPO_ROOT = HARNESS_DIR.parents[1]
sys.path.insert(0, str(HARNESS_DIR))
sys.path.insert(0, str(REPO_ROOT / "tests"))
sys.path.insert(0, str(REPO_ROOT / ".github" / "scripts"))

from analyze import fold_perf_script  # noqa: E402
from catalog import load_catalog, required_perf_for, scenario_kind  # noqa: E402
from skipprd_cli import log_args  # noqa: E402
from fixtures import (  # noqa: E402
    expected_amount_sum,
    write_csv,
    write_insert_sql,
    write_json_array,
    write_ndjson,
    write_records,
)
from parse import events_by_phase, missing_phases, parse_perf_events  # noqa: E402

PIPELINE = "perf_cell"
STORE_TABLE = "skippr-perf-catalog"
DDB_PORT = 18021
DDB_CONTAINER = "skippr-perf-ddb"
PG_APPEND_PORT = 15433
COMMAND_TIMEOUT_SECONDS = 600


class PerfError(RuntimeError):
    pass


def log(message: str) -> None:
    print(f"[skippr-perf] {message}", flush=True)


def skipprd_bin() -> Path:
    raw = os.environ.get("SKIPPRD_BIN", "").strip()
    if raw:
        path = Path(raw)
        if not path.is_file():
            raise PerfError(f"SKIPPRD_BIN is not a file: {path}")
        return path
    for candidate in (
        REPO_ROOT / "target" / "release" / "skipprd",
        REPO_ROOT / "target" / "debug" / "skipprd",
    ):
        if candidate.is_file():
            return candidate
    raise PerfError("SKIPPRD_BIN unset and target/{release,debug}/skipprd missing")


def row_count(scenario: dict[str, Any], scale: str) -> int:
    defaults = load_catalog().get("defaults", {})
    if scale == "smoke":
        return int(scenario.get("smoke_rows", defaults.get("smoke_rows", 200)))
    if scale == "linux":
        return int(scenario.get("linux_rows", defaults.get("linux_rows", 2000)))
    return int(scenario.get("rows", defaults.get("rows", 10000)))


def r2_ready() -> bool:
    return all(
        os.environ.get(name, "").strip()
        for name in ("R2_ACCOUNT_ID", "R2_ACCESS_KEY_ID", "R2_SECRET_ACCESS_KEY")
    )


def docker_ready() -> bool:
    docker = shutil.which("docker")
    if not docker:
        return False
    probe = subprocess.run([docker, "info"], capture_output=True, text=True)
    return probe.returncode == 0


def duckdb_bin() -> str:
    found = shutil.which("duckdb")
    if found:
        return found
    home = Path.home() / ".duckdb" / "cli" / "latest" / "duckdb"
    if home.is_file():
        return str(home)
    raise PerfError("duckdb CLI is required for Iceberg and parquet scan cells")


def stage_plugins(config: Path, pipeline: str, manifests: Path) -> None:
    cmd = [
        sys.executable,
        str(REPO_ROOT / ".github" / "scripts" / "local_runtime_plugins.py"),
        "--config",
        str(config),
        "--pipeline",
        pipeline,
        "--output-dir",
        str(manifests),
        "--release",
    ]
    if os.environ.get("PLUGIN_BINARIES_DIR", "").strip() or os.environ.get(
        "SKIP_CARGO_PLUGIN_BUILD", ""
    ).strip():
        cmd.append("--skip-cargo-build")
        binaries = os.environ.get("PLUGIN_BINARIES_DIR", "").strip()
        if binaries:
            dest = REPO_ROOT / "target" / "release"
            dest.mkdir(parents=True, exist_ok=True)
            for path in Path(binaries).iterdir():
                if path.name.startswith("skippr-plugin-"):
                    target = dest / path.name
                    shutil.copy2(path, target)
                    target.chmod(target.stat().st_mode | 0o111)
    subprocess.run(cmd, cwd=REPO_ROOT, check=True)


def base_env(config: Path, data_dir: Path, manifests: Path) -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "SKIPPR_CONFIG_FILE": str(config),
            "SKIPPRD_EL_STORAGE_MODE": "local",
            "DATA_DIR": str(data_dir),
            "DATA_DIR_MIN_FREE_BYTES": "0",
            "DATA_DIR_HIGH_WATERMARK_PCT": "0",
            "USE_LOCAL_PLUGIN_CODE": "1",
            "SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR": str(manifests),
            "SKIPPR_LOG_LEVEL": "perf",
            "SKIPPR_RUNTIME_LOG_LEVEL": "warn",
        }
    )
    env.pop("RUST_LOG", None)
    return env


def _linux_perf() -> str:
    perf = shutil.which("perf")
    if not perf:
        raise PerfError("perf is required for linux profile cells")
    subprocess.run(
        ["sudo", "sysctl", "-w", "kernel.perf_event_paranoid=-1"],
        check=False,
    )
    return perf


def wrap_profile(cmd: list[str], profile: str, work: Path) -> list[str]:
    if profile in {"", "none"}:
        return cmd
    if profile == "heaptrack":
        heaptrack = shutil.which("heaptrack")
        if not heaptrack:
            raise PerfError("heaptrack is required for profile=heaptrack")
        return [heaptrack, "-o", str(work / "heaptrack"), "--", *cmd]
    if profile == "perf-stat":
        perf = _linux_perf()
        return [perf, "stat", "-o", str(work / "perf-stat.txt"), "--", *cmd]
    if profile != "flamegraph":
        raise PerfError(f"unknown profile {profile!r}")
    if sys.platform.startswith("linux"):
        perf = _linux_perf()
        return [perf, "record", "-g", "-o", str(work / "perf.data"), "--", *cmd]
    samply = shutil.which("samply")
    if samply:
        return [samply, "record", "-o", str(work / "profile.json.gz"), "--save-only", "--", *cmd]
    log("samply not on PATH; running unprofiled")
    return cmd


def time_prefix() -> list[str]:
    time_bin = Path("/usr/bin/time")
    if not time_bin.is_file():
        return []
    if sys.platform == "darwin":
        return [str(time_bin), "-l"]
    return [str(time_bin), "-v"]


def run_logged(
    cmd: list[str],
    *,
    env: dict[str, str],
    cwd: Path | None = None,
    stdin_data: bytes | None = None,
    timeout: int | None = COMMAND_TIMEOUT_SECONDS,
) -> str:
    log("+ " + " ".join(cmd))
    proc = subprocess.Popen(
        cmd,
        cwd=cwd or REPO_ROOT,
        env=env,
        stdin=subprocess.PIPE if stdin_data is not None else subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        stdout, stderr = proc.communicate(
            input=stdin_data if stdin_data is not None else None,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except (ProcessLookupError, PermissionError, OSError):
            proc.kill()
        proc.wait()
        raise PerfError(f"command timed out after {timeout}s: {' '.join(cmd)}") from None
    combined = (stdout or b"") + b"\n" + (stderr or b"")
    text = combined.decode("utf-8", errors="replace")
    sys.stdout.write(text)
    if proc.returncode != 0:
        raise PerfError(f"command failed ({proc.returncode}): {' '.join(cmd)}")
    return text


def wrap_skippr_yml(script: Path) -> Path:
    if script.name == "run_s3_sink.py":
        return script.parent / "skippr_s3_sink.yml"
    return script.parent / "skippr.yml"


def write_skippr_yml(
    path: Path,
    *,
    kind: str,
    source_kind: str,
    source_path: str,
    sink: str,
    warehouse: Path | None,
    flatten: bool,
    extra_source: dict[str, str] | None = None,
    dynamodb_store: bool = False,
    ops: bool = False,
    aggregates: bool = False,
) -> None:
    """SkipprLake always needs the DynamoDB Skippr Store; other sinks opt in with `dynamodb_store`."""
    if kind.startswith("copy"):
        flatten_yaml = ""
        transform_yaml = ""
        pipeline_type = "    type: copy\n"
    else:
        flatten_yaml = "      flatten_events: yes\n" if flatten else "      flatten_events: no\n"
        extra_transform = ""
        if ops:
            extra_transform += (
                "      ops:\n"
                '        - filter: "amount > 0"\n'
                "        - project: [id, name, amount]\n"
            )
        if aggregates:
            extra_transform += (
                "      aggregates:\n"
                "        - id: name_totals\n"
                "          key_fields: [name]\n"
                "          measures:\n"
                "            amount_total: { sum: amount }\n"
                '            row_count: { count: "*" }\n'
            )
        transform_yaml = f"    transform:\n{flatten_yaml}{extra_transform}"
        pipeline_type = ""
    if source_kind == "File":
        extra = extra_source or {}
        file_format = extra.get("source_format") or "json"
        source_block = f"""    File:
      path: {source_path}
      format: {file_format}
      batch_size_bytes: 1048576
"""
    elif source_kind == "Stdin":
        source_block = """    Stdin:
      format: json
      mode: line
      batch_size_bytes: 1048576
"""
    elif source_kind == "Socket":
        source_block = f"""    Socket:
      mode: tcp
      address: {source_path}
      framing: line
      format: json
"""
    elif source_kind == "S3":
        extra = extra_source or {}
        source_block = f"""    S3:
      s3_bucket: {extra["s3_bucket"]}
      s3_prefix: {extra["s3_prefix"]}
      endpoint_url: {extra["endpoint_url"]}
      region: auto
      format: json
      batch_size_bytes: 2048000
"""
    else:
        raise PerfError(source_kind)
    if sink == "duckdb":
        uri = f"file://{warehouse}"
        sink_block = f"""    Duckdb:
      warehouse: {uri}
      table_namespace: bronze
    schema_sink: schema_sinks.local
schema_sinks:
  local:
    Duckdb:
      warehouse: {uri}
      table_namespace: bronze
"""
    elif sink == "skipprlake":
        uri = f"file://{warehouse}"
        sink_block = f"""    SkipprLake:
      warehouse: {uri}
      catalog_table: {STORE_TABLE}
      region: us-east-1
      table_namespace: bronze
      object_store:
        type: file
    schema_sink: schema_sinks.local
schema_sinks:
  local:
    SkipprLake:
      warehouse: {uri}
      catalog_table: {STORE_TABLE}
      region: us-east-1
      table_namespace: bronze
      object_store:
        type: file
"""
    elif sink == "postgres":
        sink_block = f"""    Postgres:
      host: 127.0.0.1
      port: {PG_APPEND_PORT}
      user: postgres
      password: postgres
      database: skippr_sink
      schema: public
      sslmode: disable
    schema_sink: schema_sinks.local
schema_sinks:
  local:
    Postgres:
      host: 127.0.0.1
      port: {PG_APPEND_PORT}
      user: postgres
      password: postgres
      database: skippr_sink
      schema: public
      sslmode: disable
"""
    elif sink == "file":
        output_dir = (extra_source or {}).get("output_dir", "")
        file_format = (extra_source or {}).get("format") or "jsonl"
        sink_block = f"""    File:
      output_dir: {output_dir}
      format: {file_format}
"""
    else:
        raise PerfError(sink)
    store = ""
    if sink == "skipprlake" or dynamodb_store:
        store = f"""  store:
    type: dynamodb
    name: {STORE_TABLE}
"""
    path.write_text(
        f"""skippr:
  workspace: perf
  skipprd_el_storage_mode: local
{store}
pipelines:
  {PIPELINE}:
    auto_approve: yes
    env: test
{pipeline_type}    buffer_threshold_bytes: 20000000
    buffer_threshold_seconds: 60
    data_source: data_sources.src
    data_sink: data_sinks.dst
{transform_yaml}
data_sources:
  src:
{source_block}
data_sinks:
  dst:
{sink_block}
""",
        encoding="utf-8",
    )


def start_ddb() -> None:
    from e2e_support import dynamodb_local as ddb_local

    ddb_local.start(port=DDB_PORT, tables=[STORE_TABLE], container=DDB_CONTAINER)


def stop_ddb() -> None:
    from e2e_support import dynamodb_local as ddb_local

    ddb_local.stop(port=DDB_PORT, container=DDB_CONTAINER)


def ddb_local_env(port: int, skipprlake_token: str) -> dict[str, str]:
    endpoint = f"http://127.0.0.1:{port}"
    return {
        "AWS_ENDPOINT_URL_DYNAMODB": endpoint,
        "AWS_ENDPOINT_URL": endpoint,
        "AWS_ACCESS_KEY_ID": "local",
        "AWS_SECRET_ACCESS_KEY": "local",
        "AWS_REGION": "us-east-1",
        "AWS_DEFAULT_REGION": "us-east-1",
        "AWS_EC2_METADATA_DISABLED": "true",
        "SKIPPRLAKE_TOKEN": skipprlake_token,
    }


def start_postgres(name: str, port: int, db: str, extra_args: list[str]) -> None:
    docker = shutil.which("docker")
    if not docker:
        raise PerfError("docker is required")
    subprocess.run([docker, "rm", "-f", name], check=False, capture_output=True)
    cmd = [
        docker,
        "run",
        "-d",
        "--name",
        name,
        "-e",
        "POSTGRES_PASSWORD=postgres",
        "-e",
        f"POSTGRES_DB={db}",
        "-p",
        f"{port}:5432",
        "postgres:16-alpine",
        *extra_args,
    ]
    subprocess.run(cmd, check=True)
    for _ in range(60):
        probe = subprocess.run(
            [docker, "exec", name, "psql", "-U", "postgres", "-d", db, "-c", "SELECT 1"],
            capture_output=True,
        )
        if probe.returncode == 0:
            return
        time.sleep(1)
    raise PerfError(f"postgres {name} did not become ready")


def duckdb_scan(warehouse: Path, table: str, sql: str) -> str:
    scan = warehouse / "bronze" / table
    cmd = [
        duckdb_bin(),
        "-csv",
        "-c",
        f"SET unsafe_enable_version_guessing=true; INSTALL iceberg; LOAD iceberg; {sql.format(scan=scan)}",
    ]
    out = subprocess.check_output(cmd, text=True)
    return out.strip().splitlines()[-1]


def leftover_segs(data_dir: Path) -> int:
    return sum(1 for _ in data_dir.rglob("*.seg"))


def duckdb_sql(db: Path, sql: str) -> str:
    cmd = [duckdb_bin(), str(db), "-csv", "-c", sql]
    out = subprocess.check_output(cmd, text=True)
    return out.strip().splitlines()[-1] if out.strip() else ""


def parquet_scan(output: Path, sql: str) -> str:
    files = list(output.rglob("*.parquet"))
    if not files:
        raise PerfError(f"no parquet files under {output}")
    glob = str(output / "**" / "*.parquet")
    cmd = [
        duckdb_bin(),
        "-csv",
        "-c",
        sql.format(glob=glob),
    ]
    out = subprocess.check_output(cmd, text=True)
    return out.strip().splitlines()[-1]


def convert_flamegraph(work: Path) -> None:
    perf_data = work / "perf.data"
    if not perf_data.is_file():
        return
    script = subprocess.run(
        ["perf", "script", "-i", str(perf_data)],
        capture_output=True,
        check=False,
    )
    if script.returncode != 0:
        log("perf script failed; leaving perf.data")
        return
    text = script.stdout.decode("utf-8", errors="replace")
    stacks = fold_perf_script(text)
    collapsed = work / "perf.folded"
    if stacks:
        collapsed.write_text(
            "\n".join(f"{stack} {count}" for stack, count in stacks.items()) + "\n",
            encoding="utf-8",
        )
    svg = work / "flamegraph.svg"
    inferno_flame = shutil.which("inferno-flamegraph")
    if inferno_flame and collapsed.is_file():
        flame = subprocess.run(
            [inferno_flame],
            input=collapsed.read_bytes(),
            capture_output=True,
            check=False,
        )
        if flame.returncode == 0 and flame.stdout:
            svg.write_bytes(flame.stdout)
            return
    flamegraph = shutil.which("flamegraph.pl") or shutil.which("flamegraph")
    if flamegraph and collapsed.is_file():
        flame = subprocess.run(
            [flamegraph],
            input=collapsed.read_bytes(),
            capture_output=True,
            check=False,
        )
        if flame.returncode == 0 and flame.stdout:
            svg.write_bytes(flame.stdout)


def require_profile_outputs(work: Path, profile: str) -> None:
    if profile == "flamegraph" and sys.platform.startswith("linux"):
        if not (work / "perf.folded").is_file():
            raise PerfError("flamegraph produced no perf.folded")
        if not (work / "flamegraph.svg").is_file():
            raise PerfError("flamegraph produced no flamegraph.svg")
    if profile == "heaptrack" and not any(work.glob("heaptrack*")):
        raise PerfError("heaptrack produced no profile")
    if profile == "perf-stat":
        stat = work / "perf-stat.txt"
        if not stat.is_file() or stat.stat().st_size == 0:
            raise PerfError("perf-stat produced no output")


def write_report(path: Path, payload: dict[str, Any]) -> None:
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    log(f"wrote {path}")


def assert_copy_identity(
    scenario_id: str,
    scenario: dict[str, Any],
    rows: int,
    copied: int,
    ids: set[str],
) -> None:
    if copied != rows:
        raise PerfError(f"{scenario_id}: copied {copied} != {rows}")
    if (scenario.get("asserts") or {}).get("identity"):
        expect = {str(i) for i in range(1, rows + 1)}
        if ids != expect:
            raise PerfError(f"{scenario_id}: identity ids mismatch")


def assert_perf(events: list[dict[str, Any]], required: list[str]) -> None:
    missing = missing_phases(events, required)
    if missing:
        raise PerfError(f"missing PERF phases {missing}; events={events}")


def assert_scenario_perf(captured: str, scenario: dict[str, Any]) -> list[dict[str, Any]]:
    events = parse_perf_events(captured)
    assert_perf(events, required_perf_for(load_catalog(), scenario))
    asserts = scenario.get("asserts") or {}
    grouped = events_by_phase(events)
    for key, value in asserts.items():
        if not (key.startswith("min_") and key.endswith("_events")):
            continue
        phase = key[len("min_") : -len("_events")]
        got = len(grouped.get(phase) or [])
        need = int(value)
        if got < need:
            raise PerfError(f"expected at least {need} {phase} PERF events, got {got}")
    return events


def ingest_profile_totals(events: list[dict[str, Any]]) -> dict[str, int]:
    grouped = events_by_phase(events)
    profiles = grouped.get("ingest_profile") or []
    if not profiles:
        return {
            "exact_arrow_rows": 0,
            "exact_fallback_rows": 0,
            "legacy_rows": 0,
            "rows": 0,
        }
    last = profiles[-1]
    return {
        "exact_arrow_rows": int(last.get("exact_arrow_rows") or 0),
        "exact_fallback_rows": int(last.get("exact_fallback_rows") or 0),
        "legacy_rows": int(last.get("legacy_rows") or 0),
        "rows": int(last.get("rows") or 0),
    }


def count_jsonl(output: Path) -> tuple[int, set[str]]:
    ids: set[str] = set()
    rows = 0
    for path in output.rglob("*.jsonl"):
        for line in path.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            rows += 1
            ids.add(str(json.loads(line)["id"]))
    return rows, ids


def run_skipprd(
    skipprd: Path,
    args: list[str],
    *,
    env: dict[str, str],
    work: Path,
    profile: str,
    stdin_data: bytes | None = None,
) -> str:
    prefix = [] if stdin_data is not None else time_prefix()
    cmd = prefix + wrap_profile(
        [str(skipprd), *log_args(env), *args],
        profile,
        work,
    )
    text = run_logged(cmd, env=env, stdin_data=stdin_data)
    convert_flamegraph(work)
    require_profile_outputs(work, profile)
    return text


def run_ingest(scenario_id: str, scenario: dict[str, Any], scale: str, work: Path, profile: str) -> dict[str, Any]:
    rows = row_count(scenario, scale)
    source = work / "source"
    warehouse = work / "warehouse"
    data_dir = work / "data"
    manifests = work / "manifests"
    source.mkdir(parents=True, exist_ok=True)
    warehouse.mkdir(parents=True, exist_ok=True)
    data_dir.mkdir(parents=True, exist_ok=True)
    fixture_kind = str(scenario.get("fixture", "exact_arrow"))
    flatten = bool(scenario.get("flatten_events"))
    ops = bool(scenario.get("transform_ops"))
    aggregates = bool(scenario.get("transform_aggregates"))
    source_format = str(scenario.get("source_format") or "json")
    source_ext = "csv" if source_format == "csv" else "json"
    warmup = not flatten and bool((scenario.get("asserts") or {}).get("exact_arrow_eq_total"))
    if warmup:
        write_records(source / f"warmup.{source_ext}", fixture_kind, 1, fmt=source_format)
    sink = str(scenario["sink"])
    output = work / "output"
    extra_source: dict[str, str] = {"source_format": source_format}
    if sink == "file":
        output.mkdir(parents=True, exist_ok=True)
        extra_source["output_dir"] = str(output)
        extra_source["format"] = str(scenario.get("sink_format") or "parquet")
    config = work / "skippr.yml"
    write_skippr_yml(
        config,
        kind="ingest",
        source_kind="File",
        source_path=str(source),
        sink=sink,
        warehouse=warehouse,
        flatten=flatten,
        extra_source=extra_source,
        ops=ops,
        aggregates=aggregates,
    )
    if sink == "skipprlake":
        start_ddb()
    if sink == "postgres":
        start_postgres("skippr-perf-pg", PG_APPEND_PORT, "skippr_sink", [])
    try:
        stage_plugins(config, PIPELINE, manifests)
        env = base_env(config, data_dir, manifests)
        if sink == "skipprlake":
            env.update(ddb_local_env(DDB_PORT, "skippr-perf"))
        skipprd = skipprd_bin()
        started = time.perf_counter()
        logs = ""
        logs += run_skipprd(
            skipprd,
            ["--config", str(config), "discover", "--pipeline", PIPELINE],
            env=env,
            work=work,
            profile="none",
        )
        if warmup:
            logs += run_skipprd(
                skipprd,
                ["--config", str(config), "sync", "--pipeline", PIPELINE, "--once"],
                env=env,
                work=work,
                profile="none",
            )
            (source / f"warmup.{source_ext}").unlink(missing_ok=True)
        write_records(source / f"events.{source_ext}", fixture_kind, rows, fmt=source_format)
        measured = run_skipprd(
            skipprd,
            ["--config", str(config), "sync", "--pipeline", PIPELINE, "--once"],
            env=env,
            work=work,
            profile=profile,
        )
        logs += measured
        wall_ms = int((time.perf_counter() - started) * 1000)
        events = assert_scenario_perf(measured, scenario)
        totals = ingest_profile_totals(events)
        asserts = scenario.get("asserts") or {}
        if asserts.get("exact_arrow_eq_total"):
            if totals["exact_arrow_rows"] != rows or totals["exact_fallback_rows"] != 0:
                raise PerfError(f"{scenario_id}: exact-Arrow mismatch {totals} expected rows={rows}")
        if "exact_fallback_rows" in asserts and totals["exact_fallback_rows"] != int(
            asserts["exact_fallback_rows"]
        ):
            raise PerfError(f"{scenario_id}: fallback {totals}")
        if "exact_arrow_rows" in asserts and totals["exact_arrow_rows"] != int(
            asserts["exact_arrow_rows"]
        ):
            raise PerfError(f"{scenario_id}: exact_arrow_rows {totals}")
        table = PIPELINE
        if asserts.get("bronze_count"):
            expect_count = rows + (1 if warmup else 0)
            if sink == "postgres":
                docker = shutil.which("docker")
                assert docker
                count = subprocess.check_output(
                    [
                        docker,
                        "exec",
                        "skippr-perf-pg",
                        "psql",
                        "-U",
                        "postgres",
                        "-d",
                        "skippr_sink",
                        "-Atc",
                        f"SELECT count(*) FROM public.{PIPELINE};",
                    ],
                    text=True,
                ).strip()
                if int(count) != expect_count:
                    raise PerfError(f"{scenario_id}: postgres count {count} != {expect_count}")
            elif sink == "file":
                count = parquet_scan(
                    output,
                    "SELECT count(*) FROM read_parquet('{glob}', union_by_name=true)",
                )
                if int(count) != expect_count:
                    raise PerfError(f"{scenario_id}: parquet count {count} != {expect_count}")
            else:
                count = duckdb_scan(warehouse, table, "SELECT count(*) FROM iceberg_scan('{scan}')")
                if int(count) != expect_count:
                    raise PerfError(f"{scenario_id}: bronze count {count} != {expect_count}")
        if asserts.get("amount_sum") and sink != "postgres":
            if sink == "file":
                summed = parquet_scan(
                    output,
                    "SELECT coalesce(sum(amount),0) FROM read_parquet('{glob}', union_by_name=true)",
                )
            else:
                summed = duckdb_scan(
                    warehouse, table, "SELECT coalesce(sum(amount),0) FROM iceberg_scan('{scan}')"
                )
            expect = expected_amount_sum(rows) + (10 if warmup else 0)
            if int(float(summed)) != expect:
                raise PerfError(f"{scenario_id}: amount sum {summed} != {expect}")
        if asserts.get("leftover_seg_count") is not None:
            segs = leftover_segs(data_dir)
            if segs != int(asserts["leftover_seg_count"]):
                raise PerfError(f"{scenario_id}: leftover .seg count {segs}")
        return report_payload(scenario_id, rows, wall_ms, events, logs, work)
    finally:
        if sink == "skipprlake":
            stop_ddb()
        if sink == "postgres":
            docker = shutil.which("docker")
            if docker:
                subprocess.run([docker, "rm", "-f", "skippr-perf-pg"], check=False)


def run_copy(scenario_id: str, scenario: dict[str, Any], scale: str, work: Path, profile: str) -> dict[str, Any]:
    rows = row_count(scenario, scale)
    source = work / "source"
    output = work / "output"
    data_dir = work / "data"
    manifests = work / "manifests"
    source.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
    data_dir.mkdir(parents=True, exist_ok=True)
    write_ndjson(source / "events.json", "exact_arrow", rows)
    config = work / "skippr.yml"
    write_skippr_yml(
        config,
        kind="copy",
        source_kind="File",
        source_path=str(source),
        sink="file",
        warehouse=None,
        flatten=False,
        extra_source={"output_dir": str(output)},
    )
    stage_plugins(config, PIPELINE, manifests)
    env = base_env(config, data_dir, manifests)
    skipprd = skipprd_bin()
    started = time.perf_counter()
    logs = run_skipprd(
        skipprd,
        ["--config", str(config), "sync", "--pipeline", PIPELINE, "--once"],
        env=env,
        work=work,
        profile=profile,
    )
    wall_ms = int((time.perf_counter() - started) * 1000)
    events = assert_scenario_perf(logs, scenario)
    copied, ids = count_jsonl(output)
    assert_copy_identity(scenario_id, scenario, rows, copied, ids)
    return report_payload(scenario_id, rows, wall_ms, events, logs, work)


def run_copy_stdin(scenario_id: str, scenario: dict[str, Any], scale: str, work: Path, profile: str) -> dict[str, Any]:
    rows = row_count(scenario, scale)
    output = work / "output"
    data_dir = work / "data"
    manifests = work / "manifests"
    output.mkdir(parents=True, exist_ok=True)
    data_dir.mkdir(parents=True, exist_ok=True)
    payload_path = work / "stdin.json"
    write_ndjson(payload_path, "exact_arrow", rows)
    config = work / "skippr.yml"
    write_skippr_yml(
        config,
        kind="copy",
        source_kind="Stdin",
        source_path="",
        sink="file",
        warehouse=None,
        flatten=False,
        extra_source={"output_dir": str(output)},
    )
    stage_plugins(config, PIPELINE, manifests)
    env = base_env(config, data_dir, manifests)
    skipprd = skipprd_bin()
    started = time.perf_counter()
    logs = run_skipprd(
        skipprd,
        ["--config", str(config), "sync", "--pipeline", PIPELINE, "--once"],
        env=env,
        work=work,
        profile=profile,
        stdin_data=payload_path.read_bytes(),
    )
    wall_ms = int((time.perf_counter() - started) * 1000)
    events = assert_scenario_perf(logs, scenario)
    copied, ids = count_jsonl(output)
    assert_copy_identity(scenario_id, scenario, rows, copied, ids)
    return report_payload(scenario_id, rows, wall_ms, events, logs, work)


def run_copy_socket(scenario_id: str, scenario: dict[str, Any], scale: str, work: Path, profile: str) -> dict[str, Any]:
    rows = row_count(scenario, scale)
    output = work / "output"
    data_dir = work / "data"
    manifests = work / "manifests"
    output.mkdir(parents=True, exist_ok=True)
    data_dir.mkdir(parents=True, exist_ok=True)
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    address = f"127.0.0.1:{port}"
    payload_path = work / "socket.json"
    write_ndjson(payload_path, "exact_arrow", rows)
    config = work / "skippr.yml"
    write_skippr_yml(
        config,
        kind="copy",
        source_kind="Socket",
        source_path=address,
        sink="file",
        warehouse=None,
        flatten=False,
        extra_source={"output_dir": str(output)},
    )
    stage_plugins(config, PIPELINE, manifests)
    env = base_env(config, data_dir, manifests)
    skipprd = skipprd_bin()
    cmd = time_prefix() + wrap_profile(
        [
            str(skipprd),
            *log_args(env),
            "--config",
            str(config),
            "sync",
            "--pipeline",
            PIPELINE,
            "--once",
        ],
        profile,
        work,
    )
    log("+ " + " ".join(cmd))
    proc = subprocess.Popen(
        cmd,
        cwd=REPO_ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    def send() -> None:
        deadline = time.time() + 30
        while time.time() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=1) as conn:
                    conn.sendall(payload_path.read_bytes())
                return
            except OSError:
                time.sleep(0.2)
        raise PerfError("socket source never accepted a connection")

    sender = threading.Thread(target=send, daemon=True)
    sender.start()
    started = time.perf_counter()
    out, _ = proc.communicate(timeout=300)
    wall_ms = int((time.perf_counter() - started) * 1000)
    text = (out or b"").decode("utf-8", errors="replace")
    sys.stdout.write(text)
    if proc.returncode != 0:
        raise PerfError(f"socket copy failed: {proc.returncode}")
    sender.join(timeout=5)
    logs = text
    convert_flamegraph(work)
    events = assert_scenario_perf(logs, scenario)
    copied, ids = count_jsonl(output)
    assert_copy_identity(scenario_id, scenario, rows, copied, ids)
    return report_payload(scenario_id, rows, wall_ms, events, logs, work)


def run_wrap_existing(scenario_id: str, scenario: dict[str, Any], work: Path) -> dict[str, Any]:
    script = REPO_ROOT / str(scenario["script"])
    env = os.environ.copy()
    env["SKIPPRD_PATH"] = str(skipprd_bin())
    env["COPY_WORK"] = str(work)
    env["SKIPPR_LOG_LEVEL"] = "perf"
    env.pop("RUST_LOG", None)
    if "s3" in scenario_id:
        account = os.environ["R2_ACCOUNT_ID"]
        env["R2_S3_ENDPOINT"] = f"https://{account}.r2.cloudflarestorage.com"
        env["COPY_S3_PREFIX"] = f"perf/{os.environ.get('GITHUB_RUN_ID', 'local')}/{scenario_id}/"
        env["AWS_ACCESS_KEY_ID"] = os.environ["R2_ACCESS_KEY_ID"]
        env["AWS_SECRET_ACCESS_KEY"] = os.environ["R2_SECRET_ACCESS_KEY"]
        env["AWS_DEFAULT_REGION"] = "auto"
        env["AWS_EC2_METADATA_DISABLED"] = "true"
        manifests = work / "manifests"
        yml = wrap_skippr_yml(script)
        if yml.is_file():
            pipeline = {
                "run_matrix.py": "s3_file_copy",
                "run_s3_sink.py": "file_s3_copy",
                "run.py": "s3_s3_copy",
            }.get(script.name, "")
            if pipeline:
                stage_plugins(yml, pipeline, manifests)
                env["SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR"] = str(manifests)
                env["USE_LOCAL_PLUGIN_CODE"] = "1"
    else:
        manifests = work / "manifests"
        yml = REPO_ROOT / ".github" / "actions" / "e2e" / "file_copy" / "skippr.yml"
        stage_plugins(yml, "file_copy", manifests)
        env["SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR"] = str(manifests)
        env["USE_LOCAL_PLUGIN_CODE"] = "1"
    started = time.perf_counter()
    logs = run_logged([sys.executable, str(script)], env=env)
    wall_ms = int((time.perf_counter() - started) * 1000)
    events = assert_scenario_perf(logs, scenario)
    return report_payload(scenario_id, 0, wall_ms, events, logs, work)


def run_s3_ingest(scenario_id: str, scenario: dict[str, Any], scale: str, work: Path, profile: str) -> dict[str, Any]:
    import boto3
    from botocore.config import Config as BotoConfig

    rows = row_count(scenario, scale)
    warehouse = work / "warehouse"
    data_dir = work / "data"
    manifests = work / "manifests"
    warehouse.mkdir(parents=True, exist_ok=True)
    data_dir.mkdir(parents=True, exist_ok=True)
    payload = work / "events.json"
    write_ndjson(payload, "exact_arrow", rows)
    account = os.environ["R2_ACCOUNT_ID"]
    endpoint = f"https://{account}.r2.cloudflarestorage.com"
    prefix = f"perf-ingest/{os.environ.get('GITHUB_RUN_ID', 'local')}/"
    client = boto3.client(
        "s3",
        endpoint_url=endpoint,
        aws_access_key_id=os.environ["R2_ACCESS_KEY_ID"],
        aws_secret_access_key=os.environ["R2_SECRET_ACCESS_KEY"],
        region_name="auto",
        config=BotoConfig(signature_version="s3v4"),
    )
    key = prefix + "events.json"
    client.put_object(Bucket="skippr-e2e-sample-data", Key=key, Body=payload.read_bytes())
    start_ddb()
    config = work / "skippr.yml"
    write_skippr_yml(
        config,
        kind="ingest",
        source_kind="S3",
        source_path="",
        sink="skipprlake",
        warehouse=warehouse,
        flatten=False,
        extra_source={
            "s3_bucket": "skippr-e2e-sample-data",
            "s3_prefix": prefix,
            "endpoint_url": endpoint,
        },
    )
    try:
        stage_plugins(config, PIPELINE, manifests)
        env = base_env(config, data_dir, manifests)
        env.update(
            {
                "AWS_ACCESS_KEY_ID": os.environ["R2_ACCESS_KEY_ID"],
                "AWS_SECRET_ACCESS_KEY": os.environ["R2_SECRET_ACCESS_KEY"],
                "AWS_DEFAULT_REGION": "auto",
                "AWS_EC2_METADATA_DISABLED": "true",
                "AWS_ENDPOINT_URL_DYNAMODB": f"http://127.0.0.1:{DDB_PORT}",
            }
        )
        skipprd = skipprd_bin()
        started = time.perf_counter()
        logs = run_skipprd(
            skipprd,
            ["--config", str(config), "discover", "--pipeline", PIPELINE],
            env=env,
            work=work,
            profile="none",
        )
        measured = run_skipprd(
            skipprd,
            ["--config", str(config), "sync", "--pipeline", PIPELINE, "--once"],
            env=env,
            work=work,
            profile=profile,
        )
        logs += measured
        wall_ms = int((time.perf_counter() - started) * 1000)
        events = assert_scenario_perf(measured, scenario)
        count = duckdb_scan(warehouse, PIPELINE, "SELECT count(*) FROM iceberg_scan('{scan}')")
        if int(count) != rows:
            raise PerfError(f"{scenario_id}: s3 ingest count {count} != {rows}")
        return report_payload(scenario_id, rows, wall_ms, events, logs, work)
    finally:
        stop_ddb()
        client.delete_object(Bucket="skippr-e2e-sample-data", Key=key)


def run_duckdb_ingest(
    scenario_id: str, scenario: dict[str, Any], scale: str, work: Path
) -> dict[str, Any]:
    rows = row_count(scenario, scale)
    pattern = str(scenario.get("duckdb_pattern") or "")
    fixture_kind = str(scenario.get("fixture", "exact_arrow"))
    source = work / "source"
    source.mkdir(parents=True, exist_ok=True)
    db = work / "duckdb-baseline.db"
    if pattern == "ndjson":
        path = write_ndjson(source / "events.json", fixture_kind, rows)
        sql = f"CREATE TABLE events AS SELECT * FROM read_ndjson('{path}');"
    elif pattern == "csv":
        path = write_csv(source / "events.csv", fixture_kind, rows)
        sql = f"CREATE TABLE events AS SELECT * FROM read_csv('{path}', header=true);"
    elif pattern == "json_array":
        path = write_json_array(source / "events.json", fixture_kind, rows)
        sql = f"CREATE TABLE events AS SELECT * FROM read_json('{path}', format='array');"
    elif pattern == "insert_values":
        sql_path = write_insert_sql(source / "insert.sql", rows)
        sql = f".read '{sql_path}'"
    else:
        raise PerfError(f"{scenario_id}: unknown duckdb_pattern {pattern!r}")
    env = os.environ.copy()
    started = time.perf_counter()
    captured = run_logged([duckdb_bin(), str(db), "-c", sql], env=env)
    wall_ms = int((time.perf_counter() - started) * 1000)
    logs = captured + f'\nPERF_EVENT {{"phase":"sync","elapsed_ms":{wall_ms},"rows":{rows},"bytes":0}}\n'
    events = assert_scenario_perf(logs, scenario)
    asserts = scenario.get("asserts") or {}
    if asserts.get("row_count"):
        count = duckdb_sql(db, "SELECT count(*) FROM events;")
        if int(count) != rows:
            raise PerfError(f"{scenario_id}: duckdb count {count} != {rows}")
    if asserts.get("amount_sum"):
        summed = duckdb_sql(db, "SELECT coalesce(sum(amount),0) FROM events;")
        expect = expected_amount_sum(rows)
        if int(float(summed)) != expect:
            raise PerfError(f"{scenario_id}: duckdb amount sum {summed} != {expect}")
    return report_payload(scenario_id, rows, wall_ms, events, logs, work)


def report_payload(
    scenario_id: str,
    rows: int,
    wall_ms: int,
    events: list[dict[str, Any]],
    logs: str,
    work: Path,
) -> dict[str, Any]:
    grouped = events_by_phase(events)
    sync_events = grouped.get("sync") or []
    elapsed = int(sync_events[-1]["elapsed_ms"]) if sync_events else wall_ms
    persist = grouped.get("wal_persist") or []
    compact = grouped.get("wal_compact") or []
    reclaim = grouped.get("wal_reclaim") or []
    profile = ingest_profile_totals(events)
    bytes_total = 0
    if sync_events:
        bytes_total = int(sync_events[-1].get("bytes") or 0)
    ipc_events = grouped.get("plugin_ipc") or []
    ipc = ipc_events[-1] if ipc_events else {}
    payload = {
        "scenario": scenario_id,
        "rows": rows,
        "wall_ms": wall_ms,
        "elapsed_ms": elapsed,
        "rows_per_s": (rows / (elapsed / 1000.0)) if elapsed and rows else 0,
        "bytes_per_s": (bytes_total / (elapsed / 1000.0)) if elapsed and bytes_total else 0,
        "exact_arrow_rows": profile["exact_arrow_rows"],
        "exact_fallback_rows": profile["exact_fallback_rows"],
        "legacy_rows": profile["legacy_rows"],
        "plugin_spawn_ms": ipc.get("spawn_ms"),
        "plugin_apply_ms": ipc.get("apply_ms"),
        "wal_persist_events": len(persist),
        "wal_compact_events": len(compact),
        "wal_reclaim_events": len(reclaim),
        "perf_events": events,
    }
    (work / "perf.log").write_text(logs, encoding="utf-8")
    write_report(work / "perf-report.json", payload)
    return payload


def run_scenario(scenario_id: str, scale: str, work: Path, profile: str) -> dict[str, Any]:
    catalog = load_catalog()
    scenario = catalog["scenarios"][scenario_id]
    if scenario.get("secrets") == "r2" and scenario.get("skip_if_missing_secrets") and not r2_ready():
        log(f"{scenario_id}: skipping; R2 secrets not present")
        payload = {"scenario": scenario_id, "skipped": "missing_r2_secrets"}
        write_report(work / "perf-report.json", payload)
        return payload
    if scenario.get("requires") == "docker" and not docker_ready():
        if os.environ.get("GITHUB_ACTIONS"):
            raise PerfError(f"{scenario_id}: docker required on CI")
        log(f"{scenario_id}: skipping; docker not available")
        payload = {"scenario": scenario_id, "skipped": "missing_docker"}
        write_report(work / "perf-report.json", payload)
        return payload
    kind = scenario_kind(scenario)
    if kind == "ingest":
        return run_ingest(scenario_id, scenario, scale, work, profile)
    if kind == "copy":
        return run_copy(scenario_id, scenario, scale, work, profile)
    if kind == "copy_stdin":
        return run_copy_stdin(scenario_id, scenario, scale, work, profile)
    if kind == "copy_socket":
        return run_copy_socket(scenario_id, scenario, scale, work, profile)
    if kind == "wrap_existing":
        return run_wrap_existing(scenario_id, scenario, work)
    if kind == "ingest_s3_skipprlake":
        return run_s3_ingest(scenario_id, scenario, scale, work, profile)
    if kind == "duckdb_ingest":
        return run_duckdb_ingest(scenario_id, scenario, scale, work)
    raise PerfError(f"unknown kind {kind}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="skippr-perf e2e correctness + timings")
    parser.add_argument("--scenario", required=True, help="Scenario id or comma-separated ids")
    parser.add_argument("--scale", choices=("smoke", "ci", "linux"), default="smoke")
    parser.add_argument(
        "--profile",
        choices=("none", "flamegraph", "heaptrack", "perf-stat"),
        default="none",
    )
    parser.add_argument("--work", default="")
    args = parser.parse_args(argv)
    catalog = load_catalog()
    ids = [part.strip() for part in args.scenario.split(",") if part.strip()]
    unknown = [item for item in ids if item not in catalog["scenarios"]]
    if unknown:
        raise SystemExit(f"unknown scenario(s): {unknown}")
    failures: list[str] = []
    for scenario_id in ids:
        work = Path(args.work) if args.work else Path(tempfile.mkdtemp(prefix=f"skippr-perf-{scenario_id}-"))
        work = work / scenario_id if args.work else work
        work.mkdir(parents=True, exist_ok=True)
        log(f"running {scenario_id} work={work}")
        try:
            run_scenario(scenario_id, args.scale, work, args.profile)
        except Exception as err:  # noqa: BLE001 — harness reports then continues comma-ids
            log(f"{scenario_id} FAILED: {err}")
            failures.append(scenario_id)
            (work / "perf-report.json").write_text(
                json.dumps({"scenario": scenario_id, "error": str(err)}, indent=2) + "\n",
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
    except PerfError as err:
        print(f"skippr-perf failed: {err}", file=sys.stderr)
        raise SystemExit(1)
