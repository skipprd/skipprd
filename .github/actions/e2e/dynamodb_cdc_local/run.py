#!/usr/bin/env python3
"""DynamoDB Local snapshot_then_cdc into SkipprLake (ExactOnce) and DuckDB (CdcEncoded)."""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

from botocore.config import Config as BotoConfig
from botocore.exceptions import ClientError

ACTION = Path(__file__).resolve().parent
REPO = ACTION.parents[3]
DDB_PORT = 18013
DDB_ENDPOINT = f"http://127.0.0.1:{DDB_PORT}"
SOURCE_TABLE = "skippr_dynamo_cdc"
CATALOG_TABLE = "skippr-dynamo-cdc-catalog"
CONTAINER = "skippr-dynamodb-cdc"


def fail(msg: str) -> None:
    print(msg, file=sys.stderr)
    raise SystemExit(1)


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess[str]:
    return subprocess.run(cmd, check=True, text=True, **kwargs)


def ddb_client():
    import boto3

    return boto3.client(
        "dynamodb",
        endpoint_url=DDB_ENDPOINT,
        region_name="us-east-1",
        aws_access_key_id="local",
        aws_secret_access_key="local",
        config=BotoConfig(retries={"max_attempts": 8}),
    )


def wait_ddb(client, deadline: float) -> None:
    while time.time() < deadline:
        try:
            client.list_tables()
            return
        except Exception:
            time.sleep(0.5)
    fail("DynamoDB Local did not become ready")


def recreate_table(client, name: str, attrs: list[dict], keys: list[dict], stream: bool) -> None:
    try:
        client.delete_table(TableName=name)
        client.get_waiter("table_not_exists").wait(TableName=name)
    except ClientError:
        pass
    kwargs: dict = {
        "TableName": name,
        "AttributeDefinitions": attrs,
        "KeySchema": keys,
        "BillingMode": "PAY_PER_REQUEST",
    }
    if stream:
        kwargs["StreamSpecification"] = {
            "StreamEnabled": True,
            "StreamViewType": "NEW_AND_OLD_IMAGES",
        }
    client.create_table(**kwargs)
    client.get_waiter("table_exists").wait(TableName=name)


def start_dynamodb() -> None:
    subprocess.run(["docker", "rm", "-f", CONTAINER], check=False, capture_output=True)
    run(
        [
            "docker",
            "run",
            "-d",
            "--name",
            CONTAINER,
            "-p",
            f"{DDB_PORT}:8000",
            "amazon/dynamodb-local",
            "-jar",
            "DynamoDBLocal.jar",
            "-sharedDb",
            "-inMemory",
        ]
    )
    client = ddb_client()
    wait_ddb(client, time.time() + 30)
    recreate_table(
        client,
        CATALOG_TABLE,
        [
            {"AttributeName": "PK", "AttributeType": "S"},
            {"AttributeName": "SK", "AttributeType": "S"},
        ],
        [
            {"AttributeName": "PK", "KeyType": "HASH"},
            {"AttributeName": "SK", "KeyType": "RANGE"},
        ],
        stream=False,
    )
    recreate_table(
        client,
        SOURCE_TABLE,
        [
            {"AttributeName": "id", "AttributeType": "S"},
            {"AttributeName": "rmc", "AttributeType": "S"},
        ],
        [
            {"AttributeName": "id", "KeyType": "HASH"},
            {"AttributeName": "rmc", "KeyType": "RANGE"},
        ],
        stream=True,
    )
    client.put_item(
        TableName=SOURCE_TABLE,
        Item={
            "id": {"S": "1"},
            "rmc": {"S": "eu"},
            "name": {"S": "alpha"},
        },
    )
    client.put_item(
        TableName=SOURCE_TABLE,
        Item={
            "id": {"S": "2"},
            "rmc": {"S": "us"},
            "name": {"S": "beta"},
        },
    )


def stage_plugins(config: Path, pipeline: str, manifests: Path) -> None:
    plugin_dir = os.environ.get("PLUGIN_BINARIES_DIR", "").strip()
    if not plugin_dir:
        os.environ.pop("USE_LOCAL_PLUGIN_CODE", None)
        os.environ.pop("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR", None)
        if not os.environ.get("SKIPPR_RUNTIME_PLUGIN_DIR"):
            cache = manifests.parent / "runtime-plugins"
            cache.mkdir(parents=True, exist_ok=True)
            os.environ["SKIPPR_RUNTIME_PLUGIN_DIR"] = str(cache)
        return
    release = Path("target/release")
    release.mkdir(parents=True, exist_ok=True)
    for src in Path(plugin_dir).glob("skippr-plugin-*"):
        dest = release / src.name
        shutil.copy2(src, dest)
        dest.chmod(dest.stat().st_mode | 0o111)
    manifests.mkdir(parents=True, exist_ok=True)
    run(
        [
            sys.executable,
            str(REPO / ".github" / "scripts" / "local_runtime_plugins.py"),
            "--config",
            str(config),
            "--pipeline",
            pipeline,
            "--output-dir",
            str(manifests),
            "--release",
            "--skip-cargo-build",
        ],
        cwd=REPO,
    )
    os.environ["SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR"] = str(manifests)
    os.environ["USE_LOCAL_PLUGIN_CODE"] = "1"


def skipprd_env(data_dir: Path) -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "DATA_DIR": str(data_dir),
            "DATA_DIR_MIN_FREE_BYTES": "0",
            "DATA_DIR_HIGH_WATERMARK_PCT": "0",
            "AWS_ACCESS_KEY_ID": "local",
            "AWS_SECRET_ACCESS_KEY": "local",
            "AWS_DEFAULT_REGION": "us-east-1",
            "AWS_EC2_METADATA_DISABLED": "true",
            "AWS_ENDPOINT_URL_DYNAMODB": DDB_ENDPOINT,
        }
    )
    return env


def skipprd_bin() -> str:
    path = os.environ.get("SKIPPRD_PATH")
    if not path:
        fail("SKIPPRD_PATH is required")
    return str(Path(path).resolve())


def bronze_namespace(warehouse: Path) -> str:
    bronze = warehouse / "bronze"
    if not bronze.is_dir():
        raise RuntimeError(f"no bronze dir at {bronze}")
    dirs = sorted(p.name for p in bronze.iterdir() if p.is_dir())
    if not dirs:
        raise RuntimeError(f"no Iceberg tables under {bronze}")
    return dirs[0]


def wait_iceberg_parquet(warehouse: Path, deadline: float) -> None:
    while time.time() < deadline:
        if any(warehouse.rglob("*.parquet")):
            return
        time.sleep(0.1)
    fail("SkipprLake snapshot parquet did not land")


def iceberg_scan(warehouse: Path, namespace: str, sql: str) -> str:
    scan = warehouse / "bronze" / namespace
    duckdb = shutil.which("duckdb")
    if not duckdb:
        fail("duckdb CLI is required")
    proc = run(
        [
            duckdb,
            "-csv",
            "-c",
            "SET unsafe_enable_version_guessing=true; INSTALL iceberg; LOAD iceberg; "
            + sql.replace("{scan}", str(scan)),
        ],
        capture_output=True,
    )
    return proc.stdout.strip()


def write_config(template: Path, dest: Path, replacements: dict[str, str]) -> None:
    text = template.read_text(encoding="utf-8")
    for old, new in replacements.items():
        text = text.replace(old, new)
    dest.write_text(text, encoding="utf-8")


def assert_hex_token(token: str) -> None:
    if not token or len(token) % 2 or any(c not in "0123456789abcdefABCDEF" for c in token):
        fail(f"order token is not even-length hex: {token!r}")


def csv_data_rows(scan_out: str) -> list[str]:
    lines = [line for line in scan_out.splitlines() if line.strip()]
    if lines and ("order_token" in lines[0] or "skippr_event_time" in lines[0]):
        lines = lines[1:]
    return lines


def spawn_sync(
    bin_path: str, config: Path, pipeline: str, data_dir: Path
) -> tuple[subprocess.Popen[str], list[str]]:
    proc = subprocess.Popen(
        [bin_path, "--config", str(config), "sync", "--log", "--pipeline", pipeline, "--once"],
        cwd=REPO,
        env=skipprd_env(data_dir),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    lines: list[str] = []

    def pump() -> None:
        assert proc.stdout is not None
        for line in proc.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
            lines.append(line)

    threading.Thread(target=pump, daemon=True).start()
    return proc, lines


def wait_sync_log(proc: subprocess.Popen[str], lines: list[str], needle: str, deadline: float) -> None:
    while time.time() < deadline:
        if any(needle in line for line in lines):
            return
        if proc.poll() is not None:
            fail(f"sync exited {proc.returncode} before {needle!r}")
        time.sleep(0.05)
    fail(f"did not see {needle!r} in skipprd log")


def wait_sync(proc: subprocess.Popen[str], label: str) -> None:
    try:
        code = proc.wait(timeout=120)
    except subprocess.TimeoutExpired:
        proc.kill()
        fail(f"{label} timed out")
    if code != 0:
        fail(f"{label} exited {code}")


def put_stream_items() -> None:
    client = ddb_client()
    client.put_item(
        TableName=SOURCE_TABLE,
        Item={
            "id": {"S": "3"},
            "rmc": {"S": "uk"},
            "name": {"S": "gamma"},
        },
    )
    client.put_item(
        TableName=SOURCE_TABLE,
        Item={
            "id": {"S": "1"},
            "rmc": {"S": "eu"},
            "name": {"S": "alpha-prime"},
        },
    )


def run_skipprlake(work: Path) -> None:
    warehouse = work / "lake-warehouse"
    data_dir = work / "lake-data"
    manifests = work / "lake-manifests"
    warehouse.mkdir(parents=True)
    data_dir.mkdir(parents=True)
    config = work / "skipprlake.yml"
    write_config(
        ACTION / "skipprlake.yml",
        config,
        {"LAKE_WAREHOUSE_PLACEHOLDER": f"file://{warehouse}"},
    )
    pipeline = "dynamodb_skipprlake_cdc"
    stage_plugins(config, pipeline, manifests)
    bin_path = skipprd_bin()
    snapshot, _ = spawn_sync(bin_path, config, pipeline, data_dir)
    try:
        wait_sync(snapshot, "SkipprLake snapshot sync")
    except Exception:
        snapshot.kill()
        raise
    wait_iceberg_parquet(warehouse, time.time() + 60)
    stream, logs = spawn_sync(bin_path, config, pipeline, data_dir)
    try:
        wait_sync_log(stream, logs, "Starting with", time.time() + 60)
        time.sleep(0.5)
        put_stream_items()
        wait_sync(stream, "SkipprLake stream sync")
    except Exception:
        stream.kill()
        raise

    ns = bronze_namespace(warehouse)
    count = iceberg_scan(
        warehouse,
        ns,
        "SELECT count(DISTINCT id) FROM iceberg_scan('{scan}')",
    )
    if count.splitlines()[-1].strip() != "3":
        fail(f"expected 3 distinct keys in SkipprLake Iceberg, got {count!r}")
    token_sql = (
        "SELECT _skippr_order_token, skippr_event_time, name, year, month, day "
        "FROM iceberg_scan('{scan}') WHERE id = '1'"
    )
    scan_out = iceberg_scan(warehouse, ns, token_sql)
    lines = csv_data_rows(scan_out)
    if not lines:
        fail(f"SkipprLake iceberg_scan missing data: {scan_out!r}")
    body = lines[-1].split(",")
    token, event_time, name = body[0], body[1], body[2]
    assert_hex_token(token)
    if token == "0" * len(token):
        fail("winning SkipprLake token is still the snapshot zeros")
    if not event_time or event_time in ("0", ""):
        fail(f"SkipprLake _skippr_event_time missing: {scan_out!r}")
    if name != "alpha-prime":
        fail(f"SkipprLake name={name!r}")
    if len(body) < 6:
        fail(f"SkipprLake day partition columns missing: {scan_out!r}")


def run_duckdb(work: Path) -> None:
    warehouse = work / "duckdb-warehouse"
    data_dir = work / "duckdb-data"
    manifests = work / "duckdb-manifests"
    warehouse.mkdir(parents=True)
    data_dir.mkdir(parents=True)
    config = work / "duckdb.yml"
    write_config(
        ACTION / "duckdb.yml",
        config,
        {"WAREHOUSE_URI": f"file://{warehouse}"},
    )
    pipeline = "dynamodb_duckdb_cdc"
    stage_plugins(config, pipeline, manifests)
    bin_path = skipprd_bin()
    proc = run(
        [bin_path, "--config", str(config), "sync", "--log", "--pipeline", pipeline, "--once"],
        cwd=REPO,
        env=skipprd_env(data_dir),
        timeout=120,
    )
    if proc.returncode != 0:
        fail(f"DuckDB sync exited {proc.returncode}")
    scan_out = iceberg_scan(
        warehouse,
        bronze_namespace(warehouse),
        "SELECT _skippr_order_token, skippr_event_time FROM iceberg_scan('{scan}') LIMIT 5",
    )
    data = csv_data_rows(scan_out)
    if not data:
        fail(f"DuckDB iceberg_scan had no rows: {scan_out!r}")
    token, event_time = data[-1].split(",")[:2]
    assert_hex_token(token)
    if not event_time or event_time in ("",):
        fail(f"DuckDB _skippr_event_time missing: {scan_out!r}")


def main() -> None:
    os.chdir(REPO)
    start_dynamodb()
    work = Path(tempfile.mkdtemp(prefix="skippr-dynamo-cdc-"))
    try:
        run_skipprlake(work)
        run_duckdb(work)
    finally:
        subprocess.run(["docker", "rm", "-f", CONTAINER], check=False, capture_output=True)


if __name__ == "__main__":
    main()
