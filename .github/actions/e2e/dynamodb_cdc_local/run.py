#!/usr/bin/env python3
"""DynamoDB Local snapshot_then_cdc into SkipprLake (ExactOnce) and DuckDB (CdcEncoded).

Local: from repo root,
SKIPPRD_PATH=target/release/skipprd PLUGIN_BINARIES_DIR=... \\
  python3 .github/actions/e2e/dynamodb_cdc_local/run.py
"""

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
sys.path.insert(0, str(REPO / "tests"))
sys.path.insert(0, str(REPO / "tests" / "perf"))

from ddb_cdc import (  # noqa: E402
    DDB_SEED_SNAPSHOT_ROW_COUNT,
    ICEBERG_SNAPSHOT_COUNT_SQL,
    wait_iceberg_snapshot_row_count,
)
from e2e_support import dynamodb_local as ddb_local  # noqa: E402
DDB_PORT = 18013
DDB_ENDPOINT = f"http://127.0.0.1:{DDB_PORT}"
SOURCE_TABLE = "skippr_dynamo_cdc"
CATALOG_TABLE = "skippr-dynamo-cdc-catalog"
CONTAINER = "skippr-dynamodb-cdc"
EVENT_TS_DAY_A = 1_700_000_000
EVENT_TS_DAY_B = 1_700_086_400
EVENT_TS_STREAM_UPDATE = 1_700_172_800


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


def start_dynamodb(*, seed_event_ts: bool) -> None:
    ddb_local.start(port=DDB_PORT, tables=[], container=CONTAINER)
    client = ddb_client()
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
    item_one: dict = {
        "id": {"S": "1"},
        "rmc": {"S": "eu"},
        "name": {"S": "alpha"},
    }
    item_two: dict = {
        "id": {"S": "2"},
        "rmc": {"S": "us"},
        "name": {"S": "beta"},
    }
    if seed_event_ts:
        item_one["event_ts"] = {"N": str(EVENT_TS_DAY_A)}
        item_two["event_ts"] = {"N": str(EVENT_TS_DAY_B)}
    client.put_item(TableName=SOURCE_TABLE, Item=item_one)
    client.put_item(TableName=SOURCE_TABLE, Item=item_two)


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


def bronze_parquet_count(warehouse: Path) -> int:
    ns = bronze_namespace(warehouse)
    root = warehouse / "bronze" / ns
    return sum(1 for _ in root.rglob("*.parquet"))


def assert_snapshot_day_partitions(warehouse: Path) -> None:
    n = bronze_parquet_count(warehouse)
    if n < 2:
        fail(
            f"expected at least two snapshot parquet files for distinct event_ts days, found {n}"
        )


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
    if lines and (
        "order_token" in lines[0]
        or "skippr_event_time" in lines[0]
        or "event_ts" in lines[0]
    ):
        lines = lines[1:]
    return lines


def spawn_sync(
    bin_path: str,
    config: Path,
    pipeline: str,
    data_dir: Path,
) -> tuple[subprocess.Popen[str], list[str]]:
    cmd = [bin_path, "--config", str(config), "sync", "--log", "--pipeline", pipeline]
    proc = subprocess.Popen(
        cmd,
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


def stop_sync(proc: subprocess.Popen[str]) -> None:
    if proc.poll() is not None:
        return
    proc.kill()
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        pass


def put_stream_items(*, with_event_ts: bool) -> None:
    client = ddb_client()
    gamma: dict = {
        "id": {"S": "3"},
        "rmc": {"S": "uk"},
        "name": {"S": "gamma"},
    }
    updated: dict = {
        "id": {"S": "1"},
        "rmc": {"S": "eu"},
        "name": {"S": "alpha-prime"},
    }
    if with_event_ts:
        gamma["event_ts"] = {"N": str(EVENT_TS_DAY_B)}
        updated["event_ts"] = {"N": str(EVENT_TS_STREAM_UPDATE)}
    client.put_item(TableName=SOURCE_TABLE, Item=gamma)
    client.put_item(TableName=SOURCE_TABLE, Item=updated)
    client.delete_item(
        TableName=SOURCE_TABLE,
        Key={
            "id": {"S": "2"},
            "rmc": {"S": "us"},
        },
    )


def assert_synthetic_event_time_absent(warehouse: Path, ns: str) -> None:
    out = iceberg_scan(
        warehouse,
        ns,
        "SELECT * FROM iceberg_scan('{scan}') WHERE id = '1' LIMIT 1",
    )
    lines = [ln.strip() for ln in out.splitlines() if ln.strip()]
    if not lines:
        fail("no Iceberg header row from SkipprLake")
    cols = [c.strip().lower() for c in lines[0].split(",")]
    if "event_ts" not in cols:
        fail("SkipprLake partition leg missing user event_ts column")
    if "skippr_event_time" in cols:
        fail("SkipprLake still exposes skippr_event_time after synthetic field removal")


def wait_snapshot_rows_in_lake(warehouse: Path, ns: str, deadline: float) -> None:
    def scan_fn(sql: str) -> str:
        return iceberg_scan(warehouse, ns, sql)

    wait_iceberg_snapshot_row_count(
        scan_fn,
        ICEBERG_SNAPSHOT_COUNT_SQL,
        expected=DDB_SEED_SNAPSHOT_ROW_COUNT,
        deadline=deadline,
        fail=fail,
    )


def wait_bronze_namespace(warehouse: Path, deadline: float) -> str:
    while time.time() < deadline:
        try:
            return bronze_namespace(warehouse)
        except RuntimeError:
            time.sleep(0.25)
    fail("SkipprLake bronze namespace did not appear")


def follow_cdc(
    bin_path: str,
    config: Path,
    pipeline: str,
    data_dir: Path,
    warehouse: Path,
    *,
    with_event_ts: bool,
    require_day_partitions: bool = False,
) -> str:
    stream, _ = spawn_sync(bin_path, config, pipeline, data_dir)
    try:
        wait_iceberg_parquet(warehouse, time.time() + 120)
        ns = wait_bronze_namespace(warehouse, time.time() + 120)
        wait_snapshot_rows_in_lake(warehouse, ns, time.time() + 120)
        if require_day_partitions:
            assert_snapshot_day_partitions(warehouse)
        put_stream_items(with_event_ts=with_event_ts)
        poll_stream_lake(warehouse, ns, time.time() + 120)
        return ns
    finally:
        stop_sync(stream)


def poll_stream_lake(
    warehouse: Path,
    ns: str,
    deadline: float,
    *,
    expect_distinct: str = "2",
    check_deleted_id: str = "2",
) -> None:
    while time.time() < deadline:
        try:
            count = iceberg_scan(
                warehouse,
                ns,
                "SELECT count(DISTINCT id) FROM iceberg_scan('{scan}')",
            )
            if count.splitlines()[-1].strip() != expect_distinct:
                raise ValueError("distinct count mismatch")
            deleted = iceberg_scan(
                warehouse,
                ns,
                f"SELECT count(*) FROM iceberg_scan('{{scan}}') WHERE id = '{check_deleted_id}'",
            )
            if deleted.splitlines()[-1].strip() != "0":
                raise ValueError("deleted id still present")
            return
        except (subprocess.CalledProcessError, ValueError, IndexError):
            time.sleep(0.25)
    fail("SkipprLake stream CDC did not converge before deadline")


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
    ns = follow_cdc(
        bin_path,
        config,
        pipeline,
        data_dir,
        warehouse,
        with_event_ts=True,
        require_day_partitions=True,
    )
    count = iceberg_scan(
        warehouse,
        ns,
        "SELECT count(DISTINCT id) FROM iceberg_scan('{scan}')",
    )
    if count.splitlines()[-1].strip() != "2":
        fail(f"expected 2 distinct keys in SkipprLake Iceberg after late delete, got {count!r}")
    deleted = iceberg_scan(
        warehouse,
        ns,
        "SELECT count(*) FROM iceberg_scan('{scan}') WHERE id = '2'",
    )
    if deleted.splitlines()[-1].strip() != "0":
        fail(f"expected id=2 absent after DynamoDB Remove, got {deleted!r}")
    token_sql = (
        "SELECT _skippr_order_token, event_ts, name "
        "FROM iceberg_scan('{scan}') WHERE id = '1'"
    )
    scan_out = iceberg_scan(warehouse, ns, token_sql)
    lines = csv_data_rows(scan_out)
    if not lines:
        fail(f"SkipprLake iceberg_scan missing data: {scan_out!r}")
    body = lines[-1].split(",")
    token, event_ts, name = body[0], body[1], body[2]
    assert_hex_token(token)
    if token == "0" * len(token):
        fail("winning SkipprLake token is still the snapshot zeros")
    if event_ts != str(EVENT_TS_STREAM_UPDATE):
        fail(f"SkipprLake event_ts={event_ts!r}, want {EVENT_TS_STREAM_UPDATE}")
    if name != "alpha-prime":
        fail(f"SkipprLake name={name!r}")
    assert_synthetic_event_time_absent(warehouse, ns)


def run_skipprlake_unpartitioned(work: Path) -> None:
    warehouse = work / "lake-warehouse-unpartitioned"
    data_dir = work / "lake-data-unpartitioned"
    manifests = work / "lake-manifests-unpartitioned"
    warehouse.mkdir(parents=True)
    data_dir.mkdir(parents=True)
    config = work / "skipprlake_unpartitioned.yml"
    write_config(
        ACTION / "skipprlake_unpartitioned.yml",
        config,
        {"LAKE_WAREHOUSE_PLACEHOLDER": f"file://{warehouse}"},
    )
    pipeline = "dynamodb_skipprlake_cdc_unpartitioned"
    stage_plugins(config, pipeline, manifests)
    bin_path = skipprd_bin()
    ns = follow_cdc(
        bin_path,
        config,
        pipeline,
        data_dir,
        warehouse,
        with_event_ts=False,
    )
    count = iceberg_scan(
        warehouse,
        ns,
        "SELECT count(DISTINCT id) FROM iceberg_scan('{scan}')",
    )
    if count.splitlines()[-1].strip() != "2":
        fail(f"unpartitioned: expected 2 distinct keys, got {count!r}")
    token_sql = (
        "SELECT _skippr_order_token, name FROM iceberg_scan('{scan}') WHERE id = '1'"
    )
    scan_out = iceberg_scan(warehouse, ns, token_sql)
    lines = csv_data_rows(scan_out)
    if not lines:
        fail(f"unpartitioned SkipprLake missing data: {scan_out!r}")
    token, name = lines[-1].split(",")[:2]
    assert_hex_token(token)
    if token == "0" * len(token):
        fail("unpartitioned winning token is still the snapshot zeros")
    if name != "alpha-prime":
        fail(f"unpartitioned name={name!r}")


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
    ns = follow_cdc(
        bin_path,
        config,
        pipeline,
        data_dir,
        warehouse,
        with_event_ts=False,
    )
    token_sql = (
        "SELECT _skippr_order_token, name FROM iceberg_scan('{scan}') WHERE id = '1'"
    )
    scan_out = iceberg_scan(warehouse, ns, token_sql)
    data = csv_data_rows(scan_out)
    if not data:
        fail(f"DuckDB iceberg_scan had no rows: {scan_out!r}")
    token, name = data[-1].split(",")[:2]
    assert_hex_token(token)
    if token == "0" * len(token):
        fail("winning DuckDB token is still the snapshot zeros")
    if name != "alpha-prime":
        fail(f"DuckDB name={name!r}")


def main() -> None:
    os.chdir(REPO)
    work = Path(tempfile.mkdtemp(prefix="skippr-dynamo-cdc-"))
    try:
        start_dynamodb(seed_event_ts=True)
        run_skipprlake(work)
        start_dynamodb(seed_event_ts=False)
        run_skipprlake_unpartitioned(work)
        run_duckdb(work)
    finally:
        ddb_local.stop(port=DDB_PORT, container=CONTAINER)


if __name__ == "__main__":
    main()
