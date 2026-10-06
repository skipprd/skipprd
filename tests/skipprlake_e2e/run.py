#!/usr/bin/env python3
"""SkipprLake north-star: sync bronze → serve → dbt → gold → optional sde ask.

Lake identity is Iceberg namespace.table. Serve starts before any Flight probe.
Sibling bins are env-gated: SKIPPRD_BIN, DBT_BIN, SDE_BIN.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any

HARNESS_DIR = Path(__file__).resolve().parent
REPO_ROOT = HARNESS_DIR.parents[1]
sys.path.insert(0, str(HARNESS_DIR.parent))
from e2e_support import dynamodb_local as ddb_local
from e2e_support import flight_sql

PIPELINE = "shop"
INGEST_NS = "bronze"
STORE_TABLE = "skippr-lake-e2e"
DDB_PORT = 18010
DDB_CONTAINER = "skippr-lake-e2e-ddb"
TOKEN = "skipprlake-e2e-token"
EXPECTED = json.loads((HARNESS_DIR / "expected.json").read_text(encoding="utf-8"))


class HarnessError(RuntimeError):
    pass


def log(message: str) -> None:
    print(f"[skipprlake-e2e] {message}", flush=True)


def skipprd_bin() -> Path:
    raw = os.environ.get("SKIPPRD_BIN", "").strip()
    if raw:
        path = Path(raw)
        if not path.is_file():
            raise HarnessError(f"SKIPPRD_BIN is not a file: {path}")
        return path
    built = REPO_ROOT / "target" / "debug" / "skipprd"
    if built.is_file():
        return built
    raise HarnessError("SKIPPRD_BIN unset and target/debug/skipprd missing")


def dbt_bin() -> Path | None:
    raw = os.environ.get("DBT_BIN", "").strip()
    if not raw:
        sibling = REPO_ROOT.parent / "dbt-skipprlake" / ".venv" / "bin" / "dbt"
        if sibling.is_file():
            return sibling
        return None
    path = Path(raw)
    if not path.is_file():
        raise HarnessError(f"DBT_BIN is not a file: {path}")
    return path


def sde_bin() -> Path | None:
    raw = os.environ.get("SDE_BIN", "").strip()
    if not raw:
        return None
    path = Path(raw)
    if not path.is_file():
        raise HarnessError(f"SDE_BIN is not a file: {path}")
    return path


def write_source(source_dir: Path) -> None:
    source_dir.mkdir(parents=True, exist_ok=True)
    for i in range(1, 13):
        (source_dir / f"event_{i:02d}.json").write_text(
            json.dumps({"id": i, "name": f"user_{i}", "amount": i * 10}) + "\n",
            encoding="utf-8",
        )


def write_skippr_yml(path: Path, source_dir: Path, warehouse: Path) -> None:
    warehouse_uri = f"file://{warehouse}"
    path.write_text(
        f"""skippr:
  workspace: smoke
  tenant: smoke
  skipprd_el_storage_mode: local
  store:
    type: dynamodb
    name: {STORE_TABLE}

pipelines:
  {PIPELINE}:
    auto_approve: yes
    env: test
    data_source: data_sources.orders
    data_sink: data_sinks.lake
    schema_sink: schema_sinks.lake

data_sources:
  orders:
    File:
      path: {source_dir}
      format: json
      batch_size_bytes: 1024

data_sinks:
  lake:
    SkipprLake:
      warehouse: {warehouse_uri}
      catalog_table: {STORE_TABLE}
      region: us-east-1
      table_namespace: {INGEST_NS}
      object_store:
        type: file
    schema_sink: schema_sinks.lake

schema_sinks:
  lake:
    SkipprLake:
      warehouse: {warehouse_uri}
      catalog_table: {STORE_TABLE}
      region: us-east-1
      table_namespace: {INGEST_NS}
      object_store:
        type: file
""",
        encoding="utf-8",
    )


def lake_env(config_path: Path, data_dir: Path, manifests: str) -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "AWS_ENDPOINT_URL_DYNAMODB": f"http://127.0.0.1:{DDB_PORT}",
            "AWS_ENDPOINT_URL": f"http://127.0.0.1:{DDB_PORT}",
            "AWS_ACCESS_KEY_ID": "local",
            "AWS_SECRET_ACCESS_KEY": "local",
            "AWS_REGION": "us-east-1",
            "AWS_DEFAULT_REGION": "us-east-1",
            "AWS_EC2_METADATA_DISABLED": "true",
            "SKIPPR_CONFIG_FILE": str(config_path),
            "SKIPPRD_EL_STORAGE_MODE": "local",
            "USE_LOCAL_PLUGIN_CODE": "1",
            "SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR": manifests,
            "DATA_DIR": str(data_dir),
            "DATA_DIR_HIGH_WATERMARK_PCT": "0",
            "SKIPPRLAKE_TOKEN": TOKEN,
            "RUST_LOG": "info",
        }
    )
    return env


def build_skipprd() -> Path:
    pinned = os.environ.get("SKIPPRD_BIN", "").strip()
    if pinned:
        log(f"using SKIPPRD_BIN={pinned}")
        return skipprd_bin()
    log("building skipprd --features offset-store-dynamodb")
    subprocess.run(
        ["cargo", "build", "-p", "skipprd", "--features", "offset-store-dynamodb"],
        cwd=REPO_ROOT,
        check=True,
    )
    return skipprd_bin()


def stage_plugins(config_path: Path) -> str:
    cmd = [
        sys.executable,
        str(REPO_ROOT / ".github" / "scripts" / "local_runtime_plugins.py"),
        "--config",
        str(config_path),
        "--pipeline",
        PIPELINE,
    ]
    if os.environ.get("SKIP_CARGO_BUILD", "").strip() in {"1", "true", "yes"}:
        cmd.extend(["--skip-cargo-build", "--release"])
    result = subprocess.run(
        cmd,
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    sys.stdout.write(result.stdout)
    lines = [line.strip() for line in result.stdout.splitlines() if line.strip()]
    if not lines:
        raise HarnessError("local_runtime_plugins.py printed no manifest dir")
    return lines[-1]


def run_sync(skipprd: Path, config_path: Path, env: dict[str, str]) -> None:
    log("stage sync")
    result = subprocess.run(
        [
            str(skipprd),
            "--config",
            str(config_path),
            "sync",
            "--pipeline",
            PIPELINE,
            "--once",
            "--output",
            "text",
        ],
        cwd=REPO_ROOT,
        env=env,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise HarnessError(f"sync failed:\n{result.stdout}\n{result.stderr}")


def start_serve(
    skipprd: Path, config_path: Path, env: dict[str, str], workdir: Path
) -> tuple[subprocess.Popen[str], dict[str, str]]:
    ready = workdir / "serve.ready"
    if ready.exists():
        ready.unlink()
    log_path = workdir / "serve.log"
    log_file = log_path.open("w", encoding="utf-8")
    child = subprocess.Popen(
        [
            str(skipprd),
            "--config",
            str(config_path),
            "serve",
            "--rest-bind",
            "127.0.0.1:0",
            "--flight-bind",
            "127.0.0.1:0",
            "--ready-file",
            str(ready),
        ],
        cwd=REPO_ROOT,
        env=env,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        text=True,
    )
    deadline = time.time() + 30
    while time.time() < deadline:
        if child.poll() is not None:
            raise HarnessError(f"serve exited:\n{log_path.read_text(encoding='utf-8')}")
        if ready.is_file() and ready.stat().st_size > 0:
            endpoints = json.loads(ready.read_text(encoding="utf-8"))
            if endpoints.get("rest") and endpoints.get("flight"):
                return child, endpoints
        time.sleep(0.1)
    raise HarnessError("serve ready file not written")


def write_profiles(path: Path, rest: str, flight: str) -> None:
    path.write_text(
        f"""shop:
  target: skipprlake
  outputs:
    skipprlake:
      type: skipprlake
      catalog_uri: {rest}
      query_uri: {flight}
      token: "{{{{ env_var('SKIPPRLAKE_TOKEN') }}}}"
      schema: analytics
      threads: 1
""",
        encoding="utf-8",
    )


def run_dbt(dbt: Path, project: Path, profiles: Path, env: dict[str, str]) -> None:
    result = subprocess.run(
        [
            str(dbt),
            "build",
            "--project-dir",
            str(project),
            "--profiles-dir",
            str(profiles.parent),
        ],
        env=env,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise HarnessError(f"dbt build failed:\n{result.stdout}\n{result.stderr}")


def assert_gold(flight: str) -> None:
    count = flight_sql.query_flight_count(
        flight, "SELECT COUNT(*) FROM bronze.shop", token=TOKEN
    )
    if count != EXPECTED["bronze_count"]:
        raise HarnessError(f"bronze.shop count {count} != {EXPECTED['bronze_count']}")
    table = flight_sql.query_flight_table(
        flight,
        "SELECT record_count, distinct_shop_count, amount_sum FROM shop_gold.agg_shop_summary",
        token=TOKEN,
    )
    row = table.to_pydict()
    got = {
        "record_count": int(row["record_count"][0]),
        "distinct_shop_count": int(row["distinct_shop_count"][0]),
        "amount_sum": int(row["amount_sum"][0]),
    }
    for key in ("record_count", "distinct_shop_count", "amount_sum"):
        if got[key] != EXPECTED[key]:
            raise HarnessError(f"gold {key} {got[key]} != {EXPECTED[key]}")


def atomic_replace(flight: str, dbt: Path, project: Path, profiles: Path, env: dict[str, str]) -> None:
    errors: list[str] = []
    stop = threading.Event()

    def poll() -> None:
        while not stop.is_set():
            try:
                flight_sql.query_flight_count(
                    flight, "SELECT COUNT(*) FROM shop_gold.fct_shop", token=TOKEN
                )
            except Exception as err:
                text = str(err)
                if "not found" in text.lower() or "does not exist" in text.lower():
                    errors.append(text)
            time.sleep(0.05)

    reader = threading.Thread(target=poll, daemon=True)
    reader.start()
    try:
        run_dbt(dbt, project, profiles, env)
    finally:
        stop.set()
        reader.join(timeout=5)
    if errors:
        raise HarnessError(f"atomic replace lost table: {errors[0]}")
    assert_gold(flight)


def run_functional(env: dict[str, str], rest: str, flight: str) -> None:
    adapter = REPO_ROOT.parent / "dbt-skipprlake"
    pytest = adapter / ".venv" / "bin" / "pytest"
    tests = adapter / "tests" / "functional"
    if not pytest.is_file() or not tests.is_dir():
        log("stage functional SKIP (dbt-skipprlake functional suite not installed)")
        return
    func_env = env.copy()
    func_env["SKIPPRLAKE_TEST_CATALOG_URI"] = rest
    func_env["SKIPPRLAKE_TEST_QUERY_URI"] = flight
    func_env["SKIPPRLAKE_TEST_TOKEN"] = TOKEN
    result = subprocess.run(
        [str(pytest), str(tests)],
        cwd=adapter,
        env=func_env,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise HarnessError(f"functional pytest failed:\n{result.stdout}\n{result.stderr}")


def run_live(sde: Path, workdir: Path, config_path: Path, env: dict[str, str]) -> None:
    if not os.environ.get("OPENAI_API_KEY") and not os.environ.get("ANTHROPIC_API_KEY"):
        log("stage live SKIP (no LLM key)")
        return
    project = workdir / "sde-project"
    if project.exists():
        shutil.rmtree(project)
    result = subprocess.run(
        [str(sde), "model", "--pipeline", PIPELINE, "--no-resume"],
        cwd=workdir,
        env=env,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise HarnessError(f"sde model failed:\n{result.stdout}\n{result.stderr}")
    ask = subprocess.run(
        [
            str(sde),
            "ask",
            "--pipeline",
            PIPELINE,
            "--question",
            "What is the total order amount?",
            "--output",
            "json",
        ],
        cwd=workdir,
        env=env,
        capture_output=True,
        text=True,
    )
    if ask.returncode != 0:
        raise HarnessError(f"sde ask failed:\n{ask.stdout}\n{ask.stderr}")
    if str(EXPECTED["amount_sum"]) not in ask.stdout:
        raise HarnessError(f"sde ask did not contain gold sum {EXPECTED['amount_sum']}: {ask.stdout}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    workdir = Path(tempfile.mkdtemp(prefix="skipprlake-e2e-"))
    serve: subprocess.Popen[str] | None = None
    try:
        log(f"workdir {workdir}")
        source = workdir / "source"
        warehouse = workdir / "warehouse"
        data_dir = workdir / "data"
        write_source(source)
        config = workdir / "skippr.yml"
        write_skippr_yml(config, source, warehouse)
        ddb_local.start(port=DDB_PORT, tables=[STORE_TABLE], container=DDB_CONTAINER)
        skipprd = build_skipprd()
        manifests = stage_plugins(config)
        env = lake_env(config, data_dir, manifests)
        run_sync(skipprd, config, env)
        serve, endpoints = start_serve(skipprd, config, env, workdir)
        log("stage bronze count")
        bronze = flight_sql.query_flight_count(
            endpoints["flight"], "SELECT COUNT(*) FROM bronze.shop", token=TOKEN
        )
        if bronze != EXPECTED["bronze_count"]:
            raise HarnessError(f"bronze.shop count {bronze} != {EXPECTED['bronze_count']}")
        dbt = dbt_bin()
        if dbt is None:
            log("stage dbt SKIP (DBT_BIN unset and sibling dbt-skipprlake missing)")
            return 0
        project = workdir / "dbt"
        shutil.copytree(HARNESS_DIR / "testdata" / "dbt", project)
        profiles = workdir / "profiles" / "profiles.yml"
        profiles.parent.mkdir(parents=True, exist_ok=True)
        write_profiles(profiles, endpoints["rest"], endpoints["flight"])
        log("stage dbt build")
        run_dbt(dbt, project, profiles, env)
        log("stage gold")
        assert_gold(endpoints["flight"])
        log("stage atomic replace")
        atomic_replace(endpoints["flight"], dbt, project, profiles, env)
        log("stage functional")
        run_functional(env, endpoints["rest"], endpoints["flight"])
        sde = sde_bin()
        if sde is None:
            log("stage live SKIP (SDE_BIN unset)")
        else:
            log("stage live")
            run_live(sde, workdir, config, env)
        log("ok")
        return 0
    except HarnessError as err:
        log(f"FAIL {err}")
        return 1
    finally:
        if serve is not None and serve.poll() is None:
            serve.terminate()
            try:
                serve.wait(timeout=10)
            except subprocess.TimeoutExpired:
                serve.kill()
        ddb_local.stop(port=DDB_PORT, container=DDB_CONTAINER)
        if not args.keep:
            shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
