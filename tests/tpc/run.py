#!/usr/bin/env python3
"""TPC-H Q1/Q3/Q6 vs DuckDB, vanilla DataFusion, and skipprd query."""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

QUERIES = {
    "Q1": """
SELECT l_returnflag, l_linestatus, sum(l_quantity) AS sum_qty
FROM lineitem
WHERE l_shipdate <= DATE '1998-09-02'
GROUP BY l_returnflag, l_linestatus
ORDER BY l_returnflag, l_linestatus
""",
    "Q3": """
SELECT l_orderkey, sum(l_extendedprice * (1 - l_discount)) AS revenue, o_orderdate, o_shippriority
FROM customer, orders, lineitem
WHERE c_mktsegment = 'BUILDING'
  AND c_custkey = o_custkey
  AND l_orderkey = o_orderkey
  AND o_orderdate < DATE '1995-03-15'
  AND l_shipdate > DATE '1995-03-15'
GROUP BY l_orderkey, o_orderdate, o_shippriority
ORDER BY revenue DESC, o_orderdate
LIMIT 10
""",
    "Q6": """
SELECT sum(l_extendedprice * l_discount) AS revenue
FROM lineitem
WHERE l_shipdate >= DATE '1994-01-01'
  AND l_shipdate < DATE '1995-01-01'
  AND l_discount BETWEEN 0.05 AND 0.07
  AND l_quantity < 24
""",
}


def _run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess[str]:
    print("+", " ".join(cmd), file=sys.stderr)
    proc = subprocess.run(cmd, check=False, text=True, **kwargs)
    if proc.returncode != 0:
        err = proc.stderr or ""
        if err:
            print(err, file=sys.stderr)
        proc.check_returncode()
    return proc


def parquet_glob(parquet_dir: Path, table: str) -> str:
    file_path = parquet_dir / f"{table}.parquet"
    dir_path = parquet_dir / table
    src = file_path if file_path.exists() else dir_path
    if src.is_dir():
        return f"{src.as_posix()}/*.parquet"
    return src.as_posix()


def log_disk(label: str, path: Path) -> None:
    usage = shutil.disk_usage(path)
    print(
        f"disk {label} total={usage.total} used={usage.used} free={usage.free}",
        file=sys.stderr,
    )


def drop_non_parquet(out_dir: Path) -> None:
    for path in out_dir.rglob("*"):
        if path.is_file() and path.suffix.lower() != ".parquet":
            path.unlink()


def drop_table_parquet(parquet_dir: Path, table: str) -> None:
    file_path = parquet_dir / f"{table}.parquet"
    dir_path = parquet_dir / table
    if file_path.is_file():
        file_path.unlink()
    if dir_path.is_dir():
        shutil.rmtree(dir_path)


def generate(scale: int, out_dir: Path) -> None:
    _run(
        [
            "tpcgen-cli",
            "tpch",
            "parquet",
            "-s",
            str(scale),
            "--output-dir",
            str(out_dir),
        ]
    )
    drop_non_parquet(out_dir)


def duckdb_query(parquet_dir: Path, sql: str) -> str:
    views = [
        f"CREATE VIEW {name} AS SELECT * FROM read_parquet('{parquet_glob(parquet_dir, name)}');"
        for name in ("lineitem", "orders", "customer")
    ]
    script = (
        "\n".join(views)
        + "\nCOPY ("
        + sql.strip().rstrip(";")
        + ") TO '/dev/stdout' (FORMAT CSV, HEADER false);"
    )
    proc = _run(["duckdb", "-c", script], capture_output=True)
    return proc.stdout


def datafusion_query(parquet_dir: Path, sql: str) -> str:
    from datafusion import SessionContext

    ctx = SessionContext()
    for name in ("lineitem", "orders", "customer"):
        ctx.register_parquet(name, parquet_glob(parquet_dir, name))
    batches = ctx.sql(sql).collect()
    rows = []
    for batch in batches:
        for i in range(batch.num_rows):
            rows.append(
                ",".join(str(batch.column(j)[i].as_py()) for j in range(batch.num_columns))
            )
    return "\n".join(rows) + ("\n" if rows else "")


def digest(text: str) -> str:
    normalized = "\n".join(line.strip() for line in text.strip().splitlines() if line.strip())
    return hashlib.sha256(normalized.encode()).hexdigest()


def rewrite_skipprd_sql(sql: str) -> str:
    rewritten = sql
    for table in ("customer", "orders", "lineitem"):
        rewritten = rewritten.replace(f"FROM {table}", f"FROM {table}.{table}")
        rewritten = rewritten.replace(f", {table}", f", {table}.{table}")
    return rewritten


def _link_or_copy(src: Path, dest: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    try:
        os.link(src, dest)
    except OSError:
        shutil.copy2(src, dest)


def stage_parquet_source(parquet_dir: Path, table: str, dest_dir: Path) -> None:
    dest_dir.mkdir(parents=True, exist_ok=True)
    file_path = parquet_dir / f"{table}.parquet"
    dir_path = parquet_dir / table
    if file_path.is_file():
        _link_or_copy(file_path, dest_dir / file_path.name)
        return
    if dir_path.is_dir():
        files = sorted(dir_path.glob("*.parquet"))
        if not files:
            raise SystemExit(f"no parquet files for {table} in {dir_path}")
        for path in files:
            _link_or_copy(path, dest_dir / path.name)
        return
    raise SystemExit(f"missing parquet for {table} under {parquet_dir}")


def skipprd_ingest(binary: Path, work: Path, parquet_dir: Path) -> Path:
    data = work / "data"
    warehouse = work / "warehouse"
    data.mkdir(parents=True, exist_ok=True)
    warehouse.mkdir(parents=True, exist_ok=True)
    sources = {}
    for table in ("lineitem", "orders", "customer"):
        src_dir = work / "source" / table
        stage_parquet_source(parquet_dir, table, src_dir)
        sources[table] = src_dir
    config = work / "skippr.yml"
    text = (ROOT / "tests" / "tpc" / "skippr.yml").read_text(encoding="utf-8")
    text = (
        text.replace("SOURCE_LINEITEM", str(sources["lineitem"]))
        .replace("SOURCE_ORDERS", str(sources["orders"]))
        .replace("SOURCE_CUSTOMER", str(sources["customer"]))
        .replace("WAREHOUSE_URI", f"file://{warehouse}")
    )
    config.write_text(text, encoding="utf-8")
    manifests = work / "manifests"
    _run(
        [
            sys.executable,
            str(ROOT / ".github" / "scripts" / "local_runtime_plugins.py"),
            "--config",
            str(config),
            "--output-dir",
            str(manifests),
            "--release",
            "--skip-cargo-build",
        ]
    )
    env = os.environ.copy()
    env.update(
        {
            "DATA_DIR": str(data),
            "DATA_DIR_MIN_FREE_BYTES": "0",
            "DATA_DIR_HIGH_WATERMARK_PCT": "0",
            "USE_LOCAL_PLUGIN_CODE": "1",
            "SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR": str(manifests),
        }
    )
    for pipeline in ("lineitem", "orders", "customer"):
        _run(
            [
                str(binary),
                "--config",
                str(config),
                "sync",
                "--log",
                "--pipeline",
                pipeline,
                "--once",
            ],
            env=env,
        )
        shutil.rmtree(sources[pipeline], ignore_errors=True)
        drop_table_parquet(parquet_dir, pipeline)
        log_disk(f"after {pipeline} sync", work)
    return config


def skipprd_query(binary: Path, work: Path, config: Path, sql: str) -> str:
    env = os.environ.copy()
    env.update(
        {
            "DATA_DIR": str(work / "data"),
            "DATA_DIR_MIN_FREE_BYTES": "0",
            "DATA_DIR_HIGH_WATERMARK_PCT": "0",
            "USE_LOCAL_PLUGIN_CODE": "1",
            "SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR": str(work / "manifests"),
        }
    )
    proc = _run(
        [
            str(binary),
            "--config",
            str(config),
            "query",
            "--plain",
            "--sql",
            rewrite_skipprd_sql(sql),
        ],
        env=env,
        capture_output=True,
    )
    return proc.stdout


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--scale", type=int, required=True)
    parser.add_argument("--queries", default="Q1,Q3,Q6")
    parser.add_argument("--work", type=Path)
    args = parser.parse_args()
    if args.scale < 10 and os.environ.get("SKIPPR_TPC_LAPTOP") != "1":
        raise SystemExit("TPC-H SHA battery is SF10; laptop SF1 needs SKIPPR_TPC_LAPTOP=1")
    names = [q.strip() for q in args.queries.split(",") if q.strip()]
    for name in names:
        if name not in QUERIES:
            raise SystemExit(f"unknown query {name}")
    work = args.work or Path(tempfile.mkdtemp(prefix="skippr-tpc-"))
    parquet_dir = work / "parquet"
    parquet_dir.mkdir(parents=True, exist_ok=True)
    log_disk("before generate", work)
    generate(args.scale, parquet_dir)
    log_disk("after generate", work)
    skipprd = os.environ.get("SKIPPRD_BIN")
    skipprd_config = None
    mismatches = 0
    duck_hashes: dict[str, str] = {}
    for name in names:
        sql = QUERIES[name]
        duck = duckdb_query(parquet_dir, sql)
        fusion = datafusion_query(parquet_dir, sql)
        d_hash = digest(duck)
        f_hash = digest(fusion)
        duck_hashes[name] = d_hash
        print(f"{name} duckdb={d_hash} datafusion={f_hash}")
        if d_hash != f_hash:
            print(f"{name} hash mismatch duckdb vs datafusion", file=sys.stderr)
            mismatches += 1
    if skipprd:
        skipprd_config = skipprd_ingest(Path(skipprd), work, parquet_dir)
        for name in names:
            got = skipprd_query(Path(skipprd), work, skipprd_config, QUERIES[name])
            s_hash = digest(got)
            print(f"{name} skipprd={s_hash}")
            if s_hash != duck_hashes[name]:
                print(f"{name} hash mismatch skipprd vs duckdb", file=sys.stderr)
                mismatches += 1
    raise SystemExit(mismatches)


if __name__ == "__main__":
    main()
