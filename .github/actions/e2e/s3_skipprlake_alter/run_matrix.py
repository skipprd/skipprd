#!/usr/bin/env python3
"""S3 → SkipprLake ALTER TABLE matrix. Mirrors src/sqlrt/alter_table_matrix.rs."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import boto3
from botocore.config import Config as BotoConfig

ACTION = Path(__file__).resolve().parent
BUCKET = "skippr-e2e-sample-data"

# (name, sql, expect_ok, err_contains)
FAIL_WHILE_ENABLED = (
    "fail_closed_enabled_rename",
    "ALTER TABLE s3_alter RENAME COLUMN region TO region_code",
    False,
    "DISABLED",
)

FAIL_CLOSED = [
    (
        "fail_closed_missing_column",
        "ALTER TABLE s3_alter DROP COLUMN no_such_column_xyz",
        False,
        "not found",
    ),
    (
        "fail_closed_illegal_promote_varchar",
        "ALTER TABLE s3_alter ALTER COLUMN version TYPE VARCHAR",
        False,
        "cannot promote",
    ),
    (
        "fail_closed_add_column",
        "ALTER TABLE s3_alter ADD COLUMN extra STRING",
        False,
        "ADD COLUMN",
    ),
    (
        "fail_closed_nested_record_drop",
        "ALTER TABLE s3_alter DROP COLUMN detail.pour_location",
        False,
        ("nested ALTER", "not found"),
    ),
    (
        "fail_closed_merge_same",
        "ALTER TABLE s3_alter MERGE COLUMN price INTO price",
        False,
        "must differ",
    ),
    (
        "fail_closed_rename_target_exists",
        "ALTER TABLE s3_alter RENAME COLUMN region TO version",
        False,
        "already exists",
    ),
    (
        "fail_closed_promote_missing",
        "ALTER TABLE s3_alter ALTER COLUMN no_such_column_xyz TYPE BIGINT",
        False,
        "not found",
    ),
]

SUCCESS = [
    (
        "idempotent_flatten_rename",
        "ALTER TABLE s3_alter RENAME COLUMN detail_truck_reg TO detail_truck_reg",
        True,
        None,
    ),
    (
        "rename_region_into_nested",
        "ALTER TABLE s3_alter RENAME COLUMN region TO detail.region",
        True,
        None,
    ),
    (
        "rename_nested_region_back",
        "ALTER TABLE s3_alter RENAME COLUMN detail.region TO region",
        True,
        None,
    ),
    (
        "rename_region_to_region_code",
        "ALTER TABLE s3_alter RENAME COLUMN region TO region_code",
        True,
        None,
    ),
    (
        "rename_region_code_back",
        "ALTER TABLE s3_alter RENAME COLUMN region_code TO region",
        True,
        None,
    ),
    (
        "promote_version_to_bigint",
        "ALTER TABLE s3_alter ALTER COLUMN version TYPE BIGINT",
        True,
        None,
    ),
    (
        "promote_n_short_to_integer",
        "ALTER TABLE s3_alter ALTER COLUMN n_short TYPE INTEGER",
        True,
        None,
    ),
    (
        "promote_n_byte_to_bigint",
        "ALTER TABLE s3_alter ALTER COLUMN n_byte TYPE BIGINT",
        True,
        None,
    ),
    (
        "merge_price_string_into_price",
        "ALTER TABLE s3_alter MERGE COLUMN price_string INTO price",
        True,
        None,
    ),
    (
        "merge_flatten_geofence_sibling",
        "ALTER TABLE s3_alter MERGE COLUMN detail_record.geofence_id INTO detail.geofence_id",
        True,
        None,
    ),
    (
        "drop_note",
        "ALTER TABLE s3_alter DROP COLUMN note",
        True,
        None,
    ),
]


def sql_ident(name: str) -> str:
    if any(ch in name for ch in "-."):
        return f'"{name}"'
    return name


def run(cmd: list[str], env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    print("+", " ".join(cmd), flush=True)
    return subprocess.run(cmd, check=False, env=env, text=True, capture_output=True)


def query(skipprd: str, config: Path, sql: str, env: dict[str, str]) -> tuple[bool, str]:
    proc = run(
        [skipprd, "--config", str(config), "query", "--plain", "--sql", sql],
        env,
    )
    out = (proc.stdout or "") + (proc.stderr or "")
    print(out, flush=True)
    return proc.returncode == 0, out


def expect(
    name: str,
    ok: bool,
    out: str,
    expect_ok: bool,
    needle: str | tuple[str, ...] | None,
) -> None:
    if ok != expect_ok:
        raise SystemExit(f"{name}: expected ok={expect_ok}, got ok={ok}: {out}")
    if needle:
        needles = (needle,) if isinstance(needle, str) else needle
        if not any(item in out for item in needles):
            raise SystemExit(f"{name}: output lacks {needles!r}: {out}")
    print(f"PASS {name}", flush=True)


def duckdb(scan: str, sql: str) -> str:
    full = (
        "SET unsafe_enable_version_guessing=true; INSTALL iceberg; LOAD iceberg; " + sql
    )
    proc = subprocess.run(
        ["duckdb", "-csv", "-c", full],
        check=True,
        text=True,
        capture_output=True,
    )
    return proc.stdout.strip()


def upload_wave(client, prefix: str, path: Path) -> None:
    key = f"{prefix}{path.name}"
    client.upload_file(str(path), BUCKET, key)
    print(f"uploaded s3://{BUCKET}/{key}", flush=True)


def write_config(template: Path, dest: Path, warehouse: str, endpoint: str, prefix: str) -> None:
    text = template.read_text(encoding="utf-8")
    dest.write_text(
        text.replace("LAKE_WAREHOUSE_PLACEHOLDER", warehouse)
        .replace("R2_S3_ENDPOINT_PLACEHOLDER", endpoint)
        .replace("S3_PREFIX_PLACEHOLDER", prefix),
        encoding="utf-8",
    )


def main() -> None:
    skipprd = os.environ["SKIPPRD_PATH"]
    work = Path(os.environ["ALTER_WORK"])
    warehouse = os.environ["ALTER_WAREHOUSE"]
    endpoint = os.environ["R2_S3_ENDPOINT"]
    prefix_root = os.environ["ALTER_S3_PREFIX"]
    env = os.environ.copy()
    env["DATA_DIR"] = str(work / "data")
    env["DATA_DIR_MIN_FREE_BYTES"] = "0"
    env["DATA_DIR_HIGH_WATERMARK_PCT"] = "0"

    client = boto3.client(
        "s3",
        endpoint_url=endpoint,
        region_name="auto",
        aws_access_key_id=os.environ["AWS_ACCESS_KEY_ID"],
        aws_secret_access_key=os.environ["AWS_SECRET_ACCESS_KEY"],
        config=BotoConfig(s3={"addressing_style": "path"}),
    )
    wave1 = prefix_root + "wave1/"
    wave2 = prefix_root + "wave2/"
    upload_wave(client, wave1, ACTION / "testdata" / "wave1.jsonl")
    upload_wave(client, wave2, ACTION / "testdata" / "wave2.jsonl")

    template = ACTION / "skippr.yml"
    config = work / "skippr.yml"
    write_config(template, config, warehouse, endpoint, wave1)

    sync = run(
        [skipprd, "--config", str(config), "sync", "--log", "--pipeline", "s3_alter", "--once"],
        env,
    )
    print(sync.stdout, sync.stderr, flush=True)
    if sync.returncode != 0:
        raise SystemExit(f"wave1 sync failed: {sync.returncode}")

    ok, shown = query(skipprd, config, "SHOW PIPELINE s3_alter", env)
    if not ok:
        raise SystemExit(f"SHOW PIPELINE failed: {shown}")
    namespace = "s3_alter"
    try:
        start = shown.find("{")
        payload = json.loads(shown[start:]) if start >= 0 else {}
        names = [
            ns.get("name")
            for ns in payload.get("namespaces", [])
            if not str(ns.get("name", "")).startswith("_dl_")
        ]
        if names:
            namespace = names[0]
    except json.JSONDecodeError:
        pass
    print("namespace", namespace, flush=True)
    table = (
        f'{sql_ident("s3_alter")}.{sql_ident(namespace)}'
        if namespace != "s3_alter"
        else sql_ident("s3_alter")
    )

    def bind(sql: str) -> str:
        return sql.replace("ALTER TABLE s3_alter ", f"ALTER TABLE {table} ")

    scan = str(work / "warehouse" / "bronze" / namespace)
    count = duckdb(scan, f"SELECT count(*) FROM iceberg_scan('{scan}');").splitlines()[-1]
    if count != "3":
        raise SystemExit(f"wave1 count {count!r} != 3")

    ok, out = query(skipprd, config, bind(FAIL_WHILE_ENABLED[1]), env)
    expect(FAIL_WHILE_ENABLED[0], ok, out, False, FAIL_WHILE_ENABLED[3])

    ok, out = query(skipprd, config, "DISABLE PIPELINE s3_alter", env)
    expect("disable", ok, out, True, None)

    for name, sql, expect_ok, needle in FAIL_CLOSED + SUCCESS:
        ok, out = query(skipprd, config, bind(sql), env)
        expect(name, ok, out, expect_ok, needle)

    cols = duckdb(
        scan,
        f"SELECT column_name FROM (DESCRIBE SELECT * FROM iceberg_scan('{scan}'))",
    )
    print("iceberg columns:", cols, flush=True)
    names = {line.strip() for line in cols.splitlines() if line.strip() != "column_name"}
    if "region" not in names:
        raise SystemExit(f"region missing after rename-back: {names}")
    if "region_code" in names:
        raise SystemExit(f"region_code should be gone: {names}")
    if "note" in names:
        raise SystemExit(f"note should be dropped: {names}")
    if "price_string" in names:
        raise SystemExit(f"price_string should be merged away: {names}")
    if "price" not in names:
        raise SystemExit(f"price missing: {names}")
    geofence_src = "detail_record_geofence_id"
    if geofence_src in names:
        raise SystemExit(f"{geofence_src} should be merged away: {names}")

    types = duckdb(
        scan,
        f"SELECT column_name, column_type FROM (DESCRIBE SELECT * FROM iceberg_scan('{scan}')) "
        f"WHERE column_name IN ('version','n_byte')",
    )
    print("promoted types:", types, flush=True)
    if "BIGINT" not in types and "HUGEINT" not in types:
        raise SystemExit(f"version/n_byte was not promoted: {types}")

    count = duckdb(scan, f"SELECT count(*) FROM iceberg_scan('{scan}');").splitlines()[-1]
    if count != "3":
        raise SystemExit(f"count changed during ALTER: {count}")

    ok, out = query(skipprd, config, "ENABLE PIPELINE s3_alter", env)
    expect("enable", ok, out, True, None)

    write_config(template, config, warehouse, endpoint, wave2)
    sync = run(
        [skipprd, "--config", str(config), "sync", "--log", "--pipeline", "s3_alter", "--once"],
        env,
    )
    print(sync.stdout, sync.stderr, flush=True)
    if sync.returncode != 0:
        raise SystemExit(f"wave2 sync failed: {sync.returncode}")

    count = duckdb(scan, f"SELECT count(*) FROM iceberg_scan('{scan}');").splitlines()[-1]
    if count != "5":
        raise SystemExit(f"wave2 count {count!r} != 5")
    print(json.dumps({"wave1": 3, "wave2": 5, "status": "ok"}), flush=True)


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception as exc:  # pragma: no cover
        print(exc, file=sys.stderr)
        raise SystemExit(1) from exc
