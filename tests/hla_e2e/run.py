#!/usr/bin/env python3
"""Local three-node clustered skipprd harness.

No public cloud: DynamoDB Local + file:// Iceberg warehouse + local JSONL.
Queries Iceberg snapshot UNION live WAL through Arrow Flight SQL on a ready
replica `flight_addr`. Failover uses SIGKILL. Prepared-window crash uses the named
`after_prepared` failpoint file in the e2e binary (abort after local Prepared,
before local Commit). No extra cluster knobs.

Scenarios (in order):
  bootstrap_three_nodes — n1+n2 form primary+replica
  ingest_and_query_union — JSONL into Iceberg parquet + WAL; SELECT UNION
  ballista_cluster_query — every ready flight_addr same unique ids+count; shared elected_scheduler
  dynamo_closed_epoch_asserts — Dynamo Closed=1 and wal_epoch after quorum
  query_from_every_ready_replica — clustered query succeeds (WAL may be behind)
  late_node_wal_catchup — start n3, SIGKILL sync replica, n3 catch-up, restore spare
  primary_kill_failover — SIGKILL primary; survivor steals lease and ingests batch2
  sigkill_during_ingest_no_duplicate_rows — abort primary after Prepared on batch3; unique ids
  after_local_commit_no_dupes — abort after local commit before Dynamo publish; unique ids
  schema_evolution — batch4 new field; UNION unique, no double-count
  after_replica_ack_no_dupes — abort after replica Ack, before local Committed
  after_offsets_published_no_dupes — abort after Closed=1 publish, before ingest Ack
  replica_after_prepared — replica abort after Prepared, before Ack
  after_compaction_sink — abort after Iceberg sink Ok, before Acked; no duplicate parquet
  query_during_compaction_sent — hold before sink; UNION still has new ids (Sent is not lake-visible)
  snapshot_catchup_fourth_node — compact/reclaim snapshot; empty n4 installs payloads
  sigstop_old_primary — SIGSTOP partitioned primary; late resume must not overwrite
  two_node_quorum_stall — two live nodes cannot restore quorum until a third starts
  sigterm_drain — SIGTERM primary; process exits; remaining nodes still query
  truncated_log_restart — STATE ahead of mutation.log; restarted replica refuses
  enospc_during_prepared — prepared_disk_full fail-closes without Ack
  hash_conflict_no_majority — disagreeing initialized heads do not promote
  restart_generation — restarted process UUID is a new membership SK
  donor_kill_mid_catchup — kill donor during catch-up; retry reaches head
  corrupt_replica_purge — diverged replica purges only that pipeline
  before_compaction_sink_replay — abort before Iceberg sink; replay once
  query_each_replica_socket — handshake every ready replica; WAL lists may differ
  query_retry_lagging — query still succeeds after a stopped replica (Iceberg + remaining WAL, or Iceberg-only)
  failed_drain_holds_lease — SIGTERM+SIGKILL leaves lease unreleased
  two_pipelines — one ActivePrimary per process; the other pipeline continues
  mixed_protocol_handshake — current protocol HelloOk; incompatible range rejected
  rolling_protocol_replacement — protocol-3 dummy membership is never AssignReplica
  cold_start_membership — n2 discovers n1 from Dynamo membership only
  cross_cluster_handshake — wrong cluster_hash Hello is rejected
  same_host_exclusion — identical KUBERNETES_NODE_NAME cannot form primary+replica
  clustered_rejects_at_least_once — Stdout/AtLeastOnce sink exits non-zero

Usage:
    python3 tests/hla_e2e/run.py
    python3 tests/hla_e2e/run.py --keep
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import shutil
import signal
import socket
import ssl
import struct
import subprocess
import sys
import tarfile
import time
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import boto3
from botocore.config import Config as BotoConfig
from botocore.exceptions import ClientError

REPO_ROOT = Path(__file__).resolve().parents[2]
HARNESS_DIR = Path(__file__).resolve().parent
TESTDATA = HARNESS_DIR / "testdata" / "events"
PIPELINE = "hla_events"
OFFSET_TABLE = "skippr-hla-e2e-offsets"
CATALOG_TABLE = "skippr-hla-e2e-catalog"
TENANT = "hla-e2e"
WORKSPACE = "local"
CLUSTER_ID = "hla-lake"
GOSSIP_KEY = "hla-e2e-gossip-key"
PROTOCOL_MIN = 2
PROTOCOL_MAX = 2
CURRENT_PROTOCOL = 2
DDB_PORT = 8000
DDB_ENDPOINT = f"http://127.0.0.1:{DDB_PORT}"
CONTAINER = "skippr-hla-e2e-ddb"
DDB_LOCAL_URL = "https://d1ni2b6xgvw0s0.cloudfront.net/v2.x/dynamodb_local_latest.tar.gz"
DDB_LOCAL_DIR = REPO_ROOT / ".skippr" / "dynamodb-local"
HLA_VENV = REPO_ROOT / ".skippr" / "hla-venv"
DDB_JAVA: subprocess.Popen[str] | None = None
LEASE_STEAL_SECONDS = 35
PROMOTE_WAIT_SECONDS = LEASE_STEAL_SECONDS + 180
# WAL/Iceberg namespace is the pipeline name (`hla_events`), not the File path leaf.
ROW_SQL = "SELECT id FROM hla_events ORDER BY id"
COUNT_SQL = "SELECT count(*) FROM hla_events"

SCENARIOS = [
    "bootstrap_three_nodes",
    "ingest_and_query_union",
    "ballista_cluster_query",
    "dynamo_closed_epoch_asserts",
    "query_from_every_ready_replica",
    "late_node_wal_catchup",
    "primary_kill_failover",
    "sigkill_during_ingest_no_duplicate_rows",
    "after_local_commit_no_dupes",
    "schema_evolution",
    "after_replica_ack_no_dupes",
    "after_offsets_published_no_dupes",
    "replica_after_prepared",
    "after_compaction_sink",
    "query_during_compaction_sent",
    "snapshot_catchup_fourth_node",
    "sigstop_old_primary",
    "two_node_quorum_stall",
    "sigterm_drain",
    "truncated_log_restart",
    "enospc_during_prepared",
    "hash_conflict_no_majority",
    "restart_generation",
    "donor_kill_mid_catchup",
    "corrupt_replica_purge",
    "before_compaction_sink_replay",
    "query_each_replica_socket",
    "query_retry_lagging",
    "failed_drain_holds_lease",
    "two_pipelines",
    "mixed_protocol_handshake",
    "rolling_protocol_replacement",
    "cold_start_membership",
    "cross_cluster_handshake",
    "same_host_exclusion",
    "clustered_rejects_at_least_once",
]


class HarnessError(RuntimeError):
    pass


@dataclass
class Node:
    name: str
    host: str
    data_dir: Path
    log_path: Path
    process: subprocess.Popen[str] | None = None


@dataclass
class Harness:
    workdir: Path
    skipprd: Path
    config_path: Path
    warehouse: Path
    events_dir: Path
    manifest_dir: str
    keep: bool
    tls_dir: Path
    nodes: list[Node] = field(default_factory=list)
    ddb: Any = None


def mint_cluster_tls(workdir: Path) -> Path:
    tls = workdir / "cluster_tls"
    tls.mkdir(parents=True, exist_ok=True)
    ca_key = tls / "ca.key"
    ca_pem = tls / "ca.pem"
    node_key = tls / "node.key"
    node_csr = tls / "node.csr"
    node_pem = tls / "node.pem"
    ext = tls / "san.cnf"
    ext.write_text(
        "[v3]\nsubjectAltName=DNS:skippr-cluster,DNS:localhost,IP:127.0.0.1\n",
        encoding="utf-8",
    )
    subprocess.run(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-keyout",
            str(ca_key),
            "-out",
            str(ca_pem),
            "-days",
            "1",
            "-nodes",
            "-subj",
            "/CN=skippr-hla-ca",
        ],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        [
            "openssl",
            "req",
            "-newkey",
            "rsa:2048",
            "-keyout",
            str(node_key),
            "-out",
            str(node_csr),
            "-nodes",
            "-subj",
            "/CN=skippr-cluster",
        ],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        [
            "openssl",
            "x509",
            "-req",
            "-in",
            str(node_csr),
            "-CA",
            str(ca_pem),
            "-CAkey",
            str(ca_key),
            "-CAcreateserial",
            "-out",
            str(node_pem),
            "-days",
            "1",
            "-extfile",
            str(ext),
            "-extensions",
            "v3",
        ],
        check=True,
        capture_output=True,
    )
    ca_key.unlink(missing_ok=True)
    node_csr.unlink(missing_ok=True)
    return tls


def cluster_tls_env(tls_dir: Path) -> dict[str, str]:
    return {
        "SKIPPR_CLUSTER_TLS_CA": (tls_dir / "ca.pem").read_text(encoding="utf-8"),
        "SKIPPR_CLUSTER_TLS_CERT": (tls_dir / "node.pem").read_text(encoding="utf-8"),
        "SKIPPR_CLUSTER_TLS_KEY": (tls_dir / "node.key").read_text(encoding="utf-8"),
    }


def replica_ssl_context(tls_dir: Path) -> ssl.SSLContext:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = True
    ctx.verify_mode = ssl.CERT_REQUIRED
    ctx.load_verify_locations(cafile=str(tls_dir / "ca.pem"))
    ctx.load_cert_chain(
        certfile=str(tls_dir / "node.pem"), keyfile=str(tls_dir / "node.key")
    )
    return ctx


def log(message: str) -> None:
    print(f"[hla-e2e] {message}", flush=True)


def skipprd_bin() -> Path:
    return REPO_ROOT / "target" / "debug" / "skipprd"


def write_skippr_yml(path: Path, events_dir: Path, warehouse: Path) -> None:
    warehouse_uri = f"file://{warehouse}"
    path.write_text(
        f"""skippr:
  workspace: {WORKSPACE}
  skippr_s3_bucket: skippr-hla-e2e-unused
  skipprd_el_storage_mode: local

pipelines:
  {PIPELINE}:
    auto_approve: yes
    env: test
    buffer_threshold_bytes: 256
    buffer_threshold_seconds: 1
    data_source: data_sources.local_events
    data_sink: data_sinks.iceberg_local
    schema_sink: schema_sinks.iceberg_local

data_sources:
  local_events:
    File:
      path: {events_dir}
      format: json
      batch_size_bytes: 1024

data_sinks:
  iceberg_local:
    Iceberg:
      table_namespace: hla
      table_prefix: hla
      table_location_prefix: {warehouse_uri}
      catalog:
        type: skippr
        table: {CATALOG_TABLE}
        warehouse: {warehouse_uri}
        region: us-east-1
    schema_sink: schema_sinks.iceberg_local

schema_sinks:
  iceberg_local:
    Iceberg:
      table_namespace: hla
      table_prefix: hla
      table_location_prefix: {warehouse_uri}
      catalog:
        type: skippr
        table: {CATALOG_TABLE}
        warehouse: {warehouse_uri}
        region: us-east-1
""",
        encoding="utf-8",
    )


def common_env(harness: Harness, node: Node | None = None) -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "WAL_STORAGE": "clustered",
            "SKIPPR_OFFSET_DYNAMODB_TABLE": OFFSET_TABLE,
            "AWS_ENDPOINT_URL_DYNAMODB": DDB_ENDPOINT,
            "AWS_ENDPOINT_URL": DDB_ENDPOINT,
            "AWS_ACCESS_KEY_ID": "local",
            "AWS_SECRET_ACCESS_KEY": "local",
            "AWS_REGION": "us-east-1",
            "AWS_DEFAULT_REGION": "us-east-1",
            "AWS_EC2_METADATA_DISABLED": "true",
            "TENANT": TENANT,
            "SKIPPR_CONFIG_FILE": str(harness.config_path),
            "SKIPPRD_EL_STORAGE_MODE": "local",
            "USE_LOCAL_PLUGIN_CODE": "1",
            "SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR": harness.manifest_dir,
            "RUST_LOG": "info",
            "DATA_DIR_HIGH_WATERMARK_PCT": "99",
            "DATA_DIR_LOW_WATERMARK_PCT": "90",
            "SKIPPR_CLUSTER_ID": CLUSTER_ID,
            "SKIPPR_CLUSTER_GOSSIP_KEY": GOSSIP_KEY,
            "SKIPPR_QUERY_TENANT": TENANT,
            "SKIPPR_QUERY_WORKSPACE": WORKSPACE,
        }
    )
    env.update(cluster_tls_env(harness.tls_dir))
    if node is not None:
        env["DATA_DIR"] = str(node.data_dir)
        env["KUBERNETES_NODE_NAME"] = node.host
    return env


def build_skipprd() -> Path:
    log("building skipprd --features offset-store-dynamodb")
    subprocess.run(
        [
            "cargo",
            "build",
            "-p",
            "skipprd",
            "--features",
            "offset-store-dynamodb",
        ],
        cwd=REPO_ROOT,
        check=True,
    )
    binary = skipprd_bin()
    if not binary.is_file():
        raise HarnessError(f"missing binary {binary}")
    return binary


def stage_plugins(config_path: Path) -> str:
    log("staging local File + Iceberg runtime plugins")
    result = subprocess.run(
        [
            sys.executable,
            str(REPO_ROOT / ".github" / "scripts" / "local_runtime_plugins.py"),
            "--config",
            str(config_path),
            "--pipeline",
            PIPELINE,
        ],
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


def java_bin() -> str:
    candidates = [
        Path("/opt/homebrew/opt/openjdk@21/bin/java"),
        Path("/opt/homebrew/opt/openjdk@17/bin/java"),
        Path("/usr/local/opt/openjdk@21/bin/java"),
        Path("/usr/local/opt/openjdk@17/bin/java"),
    ]
    if os.environ.get("JAVA_HOME"):
        candidates.append(Path(os.environ["JAVA_HOME"]) / "bin" / "java")
    which = shutil.which("java")
    if which:
        candidates.append(Path(which))
    for candidate in candidates:
        if not candidate.is_file():
            continue
        version = subprocess.run(
            [str(candidate), "-version"],
            capture_output=True,
            text=True,
        )
        text = version.stderr + version.stdout
        if any(
            token in text
            for token in (
                'version "17',
                'version "18',
                'version "19',
                'version "20',
                'version "21',
                'version "22',
                'version "23',
                'version "24',
                'version "25',
            )
        ):
            return str(candidate)
    raise HarnessError("DynamoDB Local requires Java 17+")


def ensure_dynamodb_local_jar() -> Path:
    jar = DDB_LOCAL_DIR / "DynamoDBLocal.jar"
    if jar.is_file():
        return jar
    DDB_LOCAL_DIR.mkdir(parents=True, exist_ok=True)
    archive = DDB_LOCAL_DIR / "dynamodb_local_latest.tar.gz"
    log(f"downloading DynamoDB Local to {archive}")
    urllib.request.urlretrieve(DDB_LOCAL_URL, archive)
    with tarfile.open(archive, "r:gz") as tar:
        tar.extractall(DDB_LOCAL_DIR)
    if not jar.is_file():
        raise HarnessError(f"DynamoDB Local jar missing after extract: {jar}")
    return jar


def wait_dynamodb_client() -> Any:
    client = boto3.client(
        "dynamodb",
        endpoint_url=DDB_ENDPOINT,
        region_name="us-east-1",
        aws_access_key_id="local",
        aws_secret_access_key="local",
        config=BotoConfig(retries={"max_attempts": 8}),
    )
    deadline = time.time() + 30
    while time.time() < deadline:
        try:
            client.list_tables()
            return client
        except Exception:
            time.sleep(0.5)
    raise HarnessError("DynamoDB Local did not become ready")


def start_dynamodb() -> Any:
    global DDB_JAVA
    log("starting DynamoDB Local")
    stop_dynamodb()
    time.sleep(0.5)
    docker = shutil.which("docker")
    if docker is not None:
        probe = subprocess.run(
            [docker, "info"],
            capture_output=True,
            text=True,
        )
        if probe.returncode == 0:
            subprocess.run([docker, "rm", "-f", CONTAINER], check=False, capture_output=True)
            subprocess.run(
                [
                    docker,
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
                ],
                check=True,
            )
            client = wait_dynamodb_client()
            ensure_tables(client)
            return client
        log("Docker daemon unavailable; using Java DynamoDB Local")
    jar = ensure_dynamodb_local_jar()
    log_path = DDB_LOCAL_DIR / "local.log"
    log_file = log_path.open("w", encoding="utf-8")
    DDB_JAVA = subprocess.Popen(
        [
            java_bin(),
            f"-Djava.library.path={jar.parent / 'DynamoDBLocal_lib'}",
            "-jar",
            str(jar),
            "-sharedDb",
            "-inMemory",
            "-port",
            str(DDB_PORT),
        ],
        cwd=jar.parent,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        text=True,
    )
    client = wait_dynamodb_client()
    ensure_tables(client)
    return client


def ensure_pk_sk_table(client: Any, name: str) -> None:
    try:
        client.delete_table(TableName=name)
        client.get_waiter("table_not_exists").wait(TableName=name)
    except ClientError:
        pass
    client.create_table(
        TableName=name,
        AttributeDefinitions=[
            {"AttributeName": "PK", "AttributeType": "S"},
            {"AttributeName": "SK", "AttributeType": "S"},
        ],
        KeySchema=[
            {"AttributeName": "PK", "KeyType": "HASH"},
            {"AttributeName": "SK", "KeyType": "RANGE"},
        ],
        BillingMode="PAY_PER_REQUEST",
    )
    client.get_waiter("table_exists").wait(TableName=name)


def ensure_tables(client: Any) -> None:
    ensure_pk_sk_table(client, OFFSET_TABLE)
    ensure_pk_sk_table(client, CATALOG_TABLE)


def stop_dynamodb() -> None:
    global DDB_JAVA
    if DDB_JAVA is not None and DDB_JAVA.poll() is None:
        DDB_JAVA.send_signal(signal.SIGTERM)
        try:
            DDB_JAVA.wait(timeout=10)
        except subprocess.TimeoutExpired:
            DDB_JAVA.send_signal(signal.SIGKILL)
            DDB_JAVA.wait(timeout=5)
        DDB_JAVA = None
    if shutil.which("docker"):
        subprocess.run(["docker", "rm", "-f", CONTAINER], check=False, capture_output=True)
    lsof = shutil.which("lsof")
    if lsof:
        probe = subprocess.run(
            [lsof, "-nP", f"-iTCP:{DDB_PORT}", "-sTCP:LISTEN", "-t"],
            capture_output=True,
            text=True,
        )
        for pid in {line.strip() for line in probe.stdout.splitlines() if line.strip()}:
            subprocess.run(["kill", "-9", pid], check=False)


def start_node(
    harness: Harness,
    node: Node,
    config_path: Path | None = None,
    pipeline: str | None = PIPELINE,
) -> None:
    node.data_dir.mkdir(parents=True, exist_ok=True)
    node.log_path.parent.mkdir(parents=True, exist_ok=True)
    log_file = node.log_path.open("wb", buffering=0)
    config = config_path or harness.config_path
    env = common_env(harness, node)
    env["SKIPPR_CONFIG_FILE"] = str(config)
    cmd = [
        str(harness.skipprd),
        "--config",
        str(config),
        "--wal-storage",
        "clustered",
        "--log",
        "info",
        "sync",
    ]
    if pipeline is not None:
        cmd.extend(["--pipeline", pipeline])
    cmd.extend(["--output", "text"])
    node.process = subprocess.Popen(
        cmd,
        cwd=REPO_ROOT,
        env=env,
        stdout=log_file,
        stderr=subprocess.STDOUT,
    )
    log(f"started {node.name} pid={node.process.pid} host={node.host}")


def stop_node(node: Node, *, kill: bool = False) -> None:
    if node.process is None or node.process.poll() is not None:
        node.process = None
        return
    if kill:
        node.process.send_signal(signal.SIGKILL)
    else:
        node.process.send_signal(signal.SIGTERM)
    try:
        node.process.wait(timeout=20)
    except subprocess.TimeoutExpired:
        node.process.send_signal(signal.SIGKILL)
        node.process.wait(timeout=5)
    node.process = None


def primary_ingest_pipelines(text: str) -> set[str]:
    found: set[str] = set()
    for line in text.splitlines():
        if "clustered primary ingest started" not in line:
            continue
        match = re.search(r"pipeline=(\S+)", line)
        if match:
            found.add(match.group(1))
    return found


def process_log(node: Node) -> str:
    """Stderr of the current process. Truncated by start_node; unlike node_log,
    this does not include skipprd*.log files left behind by earlier PIDs."""
    if node.log_path.is_file():
        return node.log_path.read_text(encoding="utf-8", errors="replace")
    return ""


def node_log(node: Node) -> str:
    chunks: list[str] = []
    if node.log_path.is_file():
        chunks.append(node.log_path.read_text(encoding="utf-8", errors="replace"))
    log_dir = node.data_dir / "logs"
    if log_dir.is_dir():
        for path in sorted(log_dir.glob("skipprd*.log")):
            chunks.append(path.read_text(encoding="utf-8", errors="replace"))
    return "\n".join(chunks)


def wait_for_log(node: Node, needle: str, timeout: float) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if needle in process_log(node):
            return
        if node.process is not None and node.process.poll() is not None:
            raise HarnessError(
                f"{node.name} exited before log '{needle}':\n{node_log(node)[-4000:]}"
            )
        time.sleep(0.5)
    raise HarnessError(
        f"{node.name} timed out waiting for '{needle}':\n{node_log(node)[-4000:]}"
    )


def wait_membership(
    harness: Harness,
    ready: int,
    timeout: float,
    nodes: int | None = None,
) -> list[dict[str, Any]]:
    want_nodes = ready if nodes is None else nodes
    deadline = time.time() + timeout
    last: list[dict[str, Any]] = []
    while time.time() < deadline:
        last = membership_rows(harness)
        ready_items = [item for item in last if item.get("ready", {}).get("BOOL") is True]
        if len(last) >= want_nodes and len(ready_items) >= ready:
            return last
        time.sleep(1)
    ready_count = sum(1 for item in last if item.get("ready", {}).get("BOOL") is True)
    raise HarnessError(
        f"membership did not reach {ready} ready of {want_nodes} node rows "
        f"(rows={len(last)} ready={ready_count})"
    )


def membership_rows(harness: Harness) -> list[dict[str, Any]]:
    pk = f"cluster#{CLUSTER_ID}"
    try:
        return harness.ddb.query(
            TableName=OFFSET_TABLE,
            KeyConditionExpression="PK = :pk AND begins_with(SK, :sk)",
            ExpressionAttributeValues={":pk": {"S": pk}, ":sk": {"S": "node#"}},
        ).get("Items", [])
    except Exception as err:
        ddb_log = ""
        log_path = DDB_LOCAL_DIR / "local.log"
        if log_path.is_file():
            ddb_log = log_path.read_text(encoding="utf-8", errors="replace")[-2000:]
        java_state = "unset"
        if DDB_JAVA is not None:
            java_state = f"poll={DDB_JAVA.poll()} pid={DDB_JAVA.pid}"
        raise HarnessError(
            f"DynamoDB query failed ({java_state}): {err}\n--- ddb log ---\n{ddb_log}"
        ) from err


def lease_item(harness: Harness, pipeline: str = PIPELINE) -> dict[str, Any] | None:
    pk = f"{TENANT}#{WORKSPACE}#{pipeline}"
    try:
        return harness.ddb.get_item(
            TableName=OFFSET_TABLE,
            Key={"PK": {"S": pk}, "SK": {"S": "lease"}},
        ).get("Item")
    except ClientError:
        return None


def attr_s(item: dict[str, Any], name: str) -> str:
    return str(item.get(name, {}).get("S", ""))


def attr_bool(item: dict[str, Any], name: str) -> bool:
    return bool(item.get(name, {}).get("BOOL", False))


def state_committed(path: Path) -> int:
    raw = path.read_bytes()
    if len(raw) != 56:
        raise HarnessError(f"STATE is {len(raw)} bytes, expected 56")
    return int.from_bytes(raw[8:16], "little")


def bump_state_committed(path: Path) -> None:
    raw = bytearray(path.read_bytes())
    if len(raw) != 56:
        raise HarnessError(f"STATE is {len(raw)} bytes, expected 56")
    committed = int.from_bytes(raw[8:16], "little") + 1
    raw[8:16] = committed.to_bytes(8, "little")
    path.write_bytes(raw)


def truncate_mutation_log_tail(path: Path) -> None:
    raw = path.read_bytes()
    if len(raw) < 8:
        raise HarnessError(f"mutation.log is {len(raw)} bytes, too small to truncate")
    keep = max(0, len(raw) - 64)
    if keep == 0:
        keep = max(0, len(raw) // 2)
    path.write_bytes(raw[:keep])
    log(f"truncated {path} from {len(raw)} to {keep} bytes")


def flip_state_hash(path: Path, byte_index: int = 55) -> None:
    raw = bytearray(path.read_bytes())
    if len(raw) != 56:
        raise HarnessError(f"STATE is {len(raw)} bytes, expected 56")
    if byte_index < 24 or byte_index >= 56:
        raise HarnessError(f"hash byte {byte_index} is outside STATE hash")
    raw[byte_index] ^= 0xFF
    path.write_bytes(raw)


def _varint(n: int) -> bytes:
    out = bytearray()
    while n > 0x7F:
        out.append((n & 0x7F) | 0x80)
        n >>= 7
    out.append(n)
    return bytes(out)


def _tag(field: int, wire: int) -> bytes:
    return _varint((field << 3) | wire)


def _pb_string(field: int, value: str) -> bytes:
    raw = value.encode()
    return _tag(field, 2) + _varint(len(raw)) + raw


def _pb_u32(field: int, value: int) -> bytes:
    return _tag(field, 0) + _varint(value)


def cluster_hash(cluster_id: str) -> str:
    return hashlib.sha256(cluster_id.encode()).hexdigest()


def encode_status_request() -> bytes:
    key = _pb_string(1, TENANT) + _pb_string(2, WORKSPACE) + _pb_string(3, PIPELINE)
    status = _tag(7, 2) + _varint(len(key)) + key
    return _tag(12, 2) + _varint(len(status)) + status


def encode_client_hello(
    *,
    cluster_id: str,
    protocol_min: int,
    protocol_max: int,
    node_id: str = "00000000-0000-0000-0000-000000000001",
) -> bytes:
    inner = b"".join(
        [
            _pb_string(1, cluster_hash(cluster_id)),
            _pb_string(2, node_id),
            _pb_u32(3, protocol_min),
            _pb_u32(4, protocol_max),
        ]
    )
    return _tag(1, 2) + _varint(len(inner)) + inner


def recvall(sock: socket.socket, n: int) -> bytes | None:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def replica_hello(
    addr: str,
    payload: bytes,
    tls_dir: Path,
    timeout: float = 5.0,
) -> bytes | None:
    host, port_s = addr.rsplit(":", 1)
    ctx = replica_ssl_context(tls_dir)
    with socket.create_connection((host, int(port_s)), timeout=timeout) as raw:
        with ctx.wrap_socket(raw, server_hostname="skippr-cluster") as sock:
            sock.settimeout(timeout)
            sock.sendall(struct.pack("<I", len(payload)) + payload)
            hdr = recvall(sock, 4)
            if hdr is None:
                return None
            (size,) = struct.unpack("<I", hdr)
            return recvall(sock, size)


def frame_field(payload: bytes) -> int | None:
    if not payload:
        return None
    n = 0
    shift = 0
    for byte in payload:
        n |= (byte & 0x7F) << shift
        if byte < 0x80:
            return n >> 3
        shift += 7
        if shift > 35:
            return None
    return None


def first_replica_addr(harness: Harness, timeout: float = 30) -> str:
    deadline = time.time() + timeout
    last = ""
    while time.time() < deadline:
        addrs: list[str] = []
        for node in live_nodes(harness):
            for line in reversed(node_log(node).splitlines()):
                match = re.search(r"clustered scheduler started replica=(\S+)", line)
                if match:
                    addrs.append(match.group(1))
                    break
        for item in membership_rows(harness):
            addr = attr_s(item, "replica_addr")
            if addr:
                addrs.append(addr)
        for addr in addrs:
            last = addr
            host, port_s = addr.rsplit(":", 1)
            try:
                with socket.create_connection((host, int(port_s)), timeout=1):
                    return addr
            except OSError:
                continue
        time.sleep(0.5)
    raise HarnessError(f"no reachable replica_addr (last={last})")


def copy_batch(harness: Harness, name: str) -> None:
    src = TESTDATA / name
    dest = harness.events_dir / name
    shutil.copyfile(src, dest)
    log(f"copied {name} -> {dest}")


def parquet_files(warehouse: Path) -> list[Path]:
    return sorted(warehouse.rglob("*.parquet"))


def wait_parquet(warehouse: Path, timeout: float) -> list[Path]:
    deadline = time.time() + timeout
    while time.time() < deadline:
        files = parquet_files(warehouse)
        if files:
            return files
        time.sleep(1)
    raise HarnessError(f"no Iceberg parquet files under {warehouse}")


def run_query(harness: Harness, sql: str, config_path: Path | None = None) -> str:
    stdout, _stderr = run_query_with_stderr(harness, sql, config_path=config_path)
    return stdout


def run_query_with_stderr(
    harness: Harness, sql: str, config_path: Path | None = None
) -> tuple[str, str]:
    query_dir = harness.workdir / "query"
    query_dir.mkdir(exist_ok=True)
    env = common_env(harness)
    env["DATA_DIR"] = str(query_dir)
    env["KUBERNETES_NODE_NAME"] = "hla-query"
    try:
        result = subprocess.run(
            [
                str(harness.skipprd),
                "--config",
                str(config_path or harness.config_path),
                "--wal-storage",
                "clustered",
                "query",
                "--log",
                "info",
                "--plain",
                "--sql",
                sql,
            ],
            cwd=REPO_ROOT,
            env=env,
            capture_output=True,
            text=True,
            timeout=120,
        )
    except subprocess.TimeoutExpired as err:
        raise HarnessError(f"query timed out after {err.timeout}s") from err
    if result.returncode != 0:
        raise HarnessError(
            f"query failed ({result.returncode}):\n{result.stdout}\n{result.stderr}"
        )
    return result.stdout, result.stderr


def _varint(n: int) -> bytes:
    out = bytearray()
    while True:
        bits = n & 0x7F
        n >>= 7
        if n:
            out.append(bits | 0x80)
        else:
            out.append(bits)
            return bytes(out)


def _proto_len_delim(tag: int, value: bytes) -> bytes:
    return bytes([(tag << 3) | 2]) + _varint(len(value)) + value


def _flight_sql_command_statement_query(sql: str) -> bytes:
    inner = _proto_len_delim(1, sql.encode())
    type_url = b"type.googleapis.com/arrow.flight.protocol.sql.CommandStatementQuery"
    return _proto_len_delim(1, type_url) + _proto_len_delim(2, inner)


def _flight():
    try:
        import pyarrow.flight as flight
        return flight
    except ImportError:
        pass
    python = HLA_VENV / "bin" / "python"
    if not python.exists():
        log("installing pyarrow into .skippr/hla-venv from PyPI")
        HLA_VENV.parent.mkdir(parents=True, exist_ok=True)
        subprocess.check_call([sys.executable, "-m", "venv", str(HLA_VENV)])
        pip_env = os.environ.copy()
        pip_env["PIP_CONFIG_FILE"] = os.devnull
        pip_env["PIP_INDEX_URL"] = "https://pypi.org/simple"
        subprocess.check_call(
            [str(HLA_VENV / "bin" / "pip"), "install", "-q", "pyarrow"],
            env=pip_env,
        )
    for site in (HLA_VENV / "lib").glob("python*/site-packages"):
        path = str(site)
        if path not in sys.path:
            sys.path.insert(0, path)
    try:
        import pyarrow.flight as flight
        return flight
    except ImportError as err:
        raise HarnessError("pyarrow is required for Flight SQL") from err


def query_flight_table(addr: str, sql: str, timeout: float = 10.0):
    flight = _flight()
    del timeout
    client = flight.FlightClient(f"grpc://{addr}")
    descriptor = flight.FlightDescriptor.for_command(_flight_sql_command_statement_query(sql))
    info = client.get_flight_info(descriptor)
    if not info.endpoints:
        raise HarnessError(f"no Flight SQL endpoint from {addr}")
    return client.do_get(info.endpoints[0].ticket).read_all()


def query_flight_ids(addr: str, sql: str, timeout: float = 10.0) -> list[str]:
    table = query_flight_table(addr, sql, timeout)
    buf = table.to_pydict()
    ids: list[str] = []
    for values in buf.values():
        for item in values:
            if item is None:
                continue
            text = str(item)
            ids.extend(m.decode() if isinstance(m, bytes) else m for m in [text] if text.startswith("evt-"))
    if not ids:
        raw = str(buf)
        ids = re.findall(r"evt-[A-Za-z0-9_-]+", raw)
    seen: list[str] = []
    for item in ids:
        if item not in seen:
            seen.append(item)
    return seen


def query_flight_count(addr: str, sql: str, timeout: float = 10.0) -> int:
    table = query_flight_table(addr, sql, timeout)
    if table.num_columns < 1 or table.num_rows < 1:
        raise HarnessError(f"count query on {addr} returned empty: {table.to_pydict()}")
    value = table.column(0)[0].as_py()
    if value is None:
        raise HarnessError(f"count query on {addr} was null")
    return int(value)


ELECTED_SCHEDULER_RE = re.compile(r"elected_scheduler=([0-9.:]+)")
FLIGHT_STARTED_RE = re.compile(r"clustered scheduler started .* flight=([0-9.]+):(\d+)")


def last_elected_scheduler(node: Node) -> str | None:
    matches = ELECTED_SCHEDULER_RE.findall(node_log(node))
    return matches[-1] if matches else None


def last_flight_addr(node: Node) -> str | None:
    found = None
    for line in node_log(node).splitlines():
        match = FLIGHT_STARTED_RE.search(line)
        if match:
            found = f"{match.group(1)}:{match.group(2)}"
    return found


def wait_same_elected_scheduler(nodes: list[Node], timeout: float) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        values = [last_elected_scheduler(node) for node in nodes]
        present = [item for item in values if item]
        if len(present) == len(nodes) and len(set(present)) == 1:
            return present[0]
        time.sleep(0.5)
    raise HarnessError(
        "nodes did not share elected_scheduler: "
        + str([last_elected_scheduler(node) for node in nodes])
    )


def parse_ids(stdout: str) -> list[str]:
    ids: list[str] = []
    for line in stdout.splitlines():
        line = line.strip()
        if not line or line.lower() == "id" or line.lower().startswith("count"):
            continue
        ids.append(line.split(",")[0])
    return ids


def parse_count(stdout: str) -> int:
    for line in reversed(stdout.splitlines()):
        line = line.strip()
        if line.isdigit():
            return int(line)
    raise HarnessError(f"could not parse count from:\n{stdout}")


def pipeline_root(node: Node, pipeline: str = PIPELINE) -> Path:
    return node.data_dir / "clustered" / TENANT / WORKSPACE / pipeline


def failpoint_path(node: Node) -> Path:
    return pipeline_root(node) / "failpoint"


def mutation_log_path(node: Node, pipeline: str = PIPELINE) -> Path:
    return pipeline_root(node, pipeline) / "segment_buffer" / "durable" / "mutation.log"


def state_file_path(node: Node, pipeline: str = PIPELINE) -> Path:
    return pipeline_root(node, pipeline) / "segment_buffer" / "durable" / "STATE"


def wipe_pipeline_data(node: Node, pipeline: str = PIPELINE) -> None:
    root = pipeline_root(node, pipeline)
    if root.exists():
        shutil.rmtree(root)
        log(f"wiped {root}")


def arm_failpoint(node: Node, name: str) -> Path:
    path = failpoint_path(node)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(f"{name}\n", encoding="utf-8")
    log(f"armed {name} at {path}")
    return path


def arm_after_prepared(node: Node) -> Path:
    return arm_failpoint(node, "after_prepared")


def wait_node_exit(node: Node, timeout: float) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if node.process is None or node.process.poll() is not None:
            code = None if node.process is None else node.process.returncode
            log(f"{node.name} exited after failpoint code={code}")
            node.process = None
            return
        time.sleep(0.2)
    raise HarnessError(
        f"{node.name} did not abort after failpoint:\n{node_log(node)[-4000:]}"
    )


def wait_promoted(
    harness: Harness,
    excluded: Node,
    timeout: float,
    since: dict[str, int] | None = None,
) -> Node:
    survivors = [node for node in harness.nodes if node is not excluded]
    marks = since if since is not None else {
        node.name: len(process_log(node)) for node in survivors
    }
    deadline = time.time() + timeout
    while time.time() < deadline:
        for node in survivors:
            if node.process is None or node.process.poll() is not None:
                continue
            added = process_log(node)[marks.get(node.name, 0) :]
            if "clustered primary ingest started" in added:
                return node
        time.sleep(1)
    raise HarnessError(
        "no survivor became primary\n"
        + "\n".join(f"--- {n.name} ---\n{node_log(n)[-2000:]}" for n in survivors)
    )


def wait_unique_ids(
    harness: Harness,
    expected: set[str],
    timeout: float,
    allowed_extra: set[str] | None = None,
) -> list[str]:
    last = ""
    allowed = allowed_extra or set()
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            last = run_query(harness, ROW_SQL)
            got = parse_ids(last)
            got_set = set(got)
            extra = got_set - expected
            if expected <= got_set and extra <= allowed and len(got) == len(got_set):
                count = parse_count(run_query(harness, COUNT_SQL))
                if count != len(got):
                    raise HarnessError(f"count {count} != unique ids {len(got)}")
                return got
        except HarnessError as err:
            last = str(err)
        time.sleep(3)
    raise HarnessError(f"query never reached {sorted(expected)}:\n{last}")


def offset_closed(payload_b64: str) -> int:
    raw = base64.b64decode(payload_b64)
    if len(raw) != 24:
        raise HarnessError(f"offset payload must be 24 bytes, got {len(raw)}")
    return int.from_bytes(raw[16:24], "little")


def assert_dynamo_closed(harness: Harness) -> None:
    log("scenario dynamo_closed_epoch_asserts")
    pk = f"{TENANT}#{WORKSPACE}#{PIPELINE}"
    items = harness.ddb.query(
        TableName=OFFSET_TABLE,
        KeyConditionExpression="PK = :pk AND begins_with(SK, :sk)",
        ExpressionAttributeValues={":pk": {"S": pk}, ":sk": {"S": "offset#"}},
    ).get("Items", [])
    if not items:
        raise HarnessError(f"no Dynamo offset rows for {pk}")
    epochs: list[int] = []
    closed = 0
    for item in items:
        if "wal_epoch" not in item:
            raise HarnessError(f"offset row missing wal_epoch: {item}")
        epochs.append(int(item["wal_epoch"]["N"]))
        payload = item.get("payload_b64", {}).get("S", "")
        if not payload:
            raise HarnessError(f"offset row missing payload_b64: {item}")
        if offset_closed(payload) == 1:
            closed += 1
    if max(epochs) < 1:
        raise HarnessError(f"wal_epoch not advanced: {epochs}")
    if closed == 0:
        raise HarnessError(f"no offset row has Closed=1: {items}")
    log(f"dynamo offsets={len(items)} closed={closed} max_wal_epoch={max(epochs)}")


def restart_dead_nodes(harness: Harness) -> None:
    for node in harness.nodes:
        if node.process is None or node.process.poll() is not None:
            start_node(harness, node)
            wait_for_log(node, "clustered scheduler started", 60)
    wait_live_primary(harness, 90)


CORE_NODE_NAMES = {"n1", "n2", "n3"}


def pin_core_cluster(harness: Harness) -> list[Node]:
    """n4 (and any extra hosts) must not steal replica assignment from n1–n3."""
    core = [node for node in harness.nodes if node.name in CORE_NODE_NAMES]
    for node in list(live_nodes(harness)):
        if node.name not in CORE_NODE_NAMES:
            log(f"SIGKILL extra {node.name} so the core cluster is three nodes")
            stop_node(node, kill=True)
    for node in core:
        if node.process is None or node.process.poll() is not None:
            start_node(harness, node)
            wait_for_log(node, "clustered scheduler started", 60)
    wait_live_primary(harness, 90)
    return core


def wait_assigned_replica(harness: Harness, primary: Node, timeout: float) -> Node:
    deadline = time.time() + timeout
    while time.time() < deadline:
        endpoint = last_assigned_replica_endpoint(primary)
        if endpoint:
            replica = node_for_replica_endpoint(harness, endpoint)
            if replica is not None and replica is not primary:
                return replica
        time.sleep(0.5)
    raise HarnessError(f"primary {primary.name} did not assign a live replica")


def live_nodes(harness: Harness) -> list[Node]:
    return [node for node in harness.nodes if node.process is not None and node.process.poll() is None]


def latest_primary_started_at(node: Node) -> str | None:
    latest: str | None = None
    for line in process_log(node).splitlines():
        if "clustered primary ingest started" not in line:
            continue
        match = re.match(r"^(\d{4}-\d{2}-\d{2}T\S+)", line)
        ts = match.group(1) if match else line
        if latest is None or ts >= latest:
            latest = ts
    return latest


def current_primary(harness: Harness) -> Node | None:
    best: Node | None = None
    best_ts = ""
    for node in live_nodes(harness):
        ts = latest_primary_started_at(node)
        if ts is not None and ts >= best_ts:
            best_ts = ts
            best = node
    return best


def wait_live_primary(harness: Harness, timeout: float) -> Node:
    deadline = time.time() + timeout
    last = "no live nodes"
    while time.time() < deadline:
        live = live_nodes(harness)
        node = current_primary(harness)
        if node is not None:
            return node
        last = "\n".join(
            f"--- {node.name} ---\n{node_log(node)[-1500:]}" for node in live
        )
        time.sleep(1)
    raise HarnessError("no live primary found\n" + last)


def nodes_with_state(harness: Harness) -> list[Node]:
    return [node for node in harness.nodes if state_file_path(node).is_file()]


def nodes_with_committed_state(harness: Harness) -> list[Node]:
    got: list[Node] = []
    for node in harness.nodes:
        path = state_file_path(node)
        if path.is_file() and state_committed(path) > 0:
            got.append(node)
    return got


def wait_state_quorum(harness: Harness, need: int, timeout: float) -> list[Node]:
    deadline = time.time() + timeout
    got: list[Node] = []
    while time.time() < deadline:
        got = nodes_with_state(harness)
        if len(got) >= need:
            return got
        time.sleep(1)
    raise HarnessError(
        f"need {need} STATE files, have {[node.name for node in got]}"
    )


def wait_committed_state_quorum(harness: Harness, need: int, timeout: float) -> list[Node]:
    deadline = time.time() + timeout
    got: list[Node] = []
    while time.time() < deadline:
        got = nodes_with_committed_state(harness)
        if len(got) >= need:
            return got
        time.sleep(1)
    raise HarnessError(
        f"need {need} STATE files with committed>0, have {[node.name for node in got]}"
    )


def seed_wal_state(harness: Harness) -> None:
    seed = harness.events_dir / "purge-seed.jsonl"
    if seed.is_file():
        return
    seed.write_text(
        '{"id":"evt-purge-seed","event_type":"signup","amount":1}\n',
        encoding="utf-8",
    )
    log("wrote purge-seed.jsonl so clustered replicas persist STATE")


def corrupt_diverged_snapshot_head(replica: Node) -> None:
    state = state_file_path(replica)
    log_path = mutation_log_path(replica)
    log_path.parent.mkdir(parents=True, exist_ok=True)
    raw = bytearray(state.read_bytes())
    if len(raw) != 56:
        raise HarnessError(f"STATE is {len(raw)} bytes, expected 56")
    committed = int.from_bytes(raw[8:16], "little")
    if committed == 0:
        raise HarnessError(f"{replica.name} STATE is still genesis")
    raw[0:8] = committed.to_bytes(8, "little")
    raw[16:24] = committed.to_bytes(8, "little")
    state.write_bytes(raw)
    flip_state_hash(state)
    log_path.write_bytes(b"")
    log(
        f"corrupted {replica.name} STATE committed={committed} "
        "to snapshot-boundary hash mismatch"
    )


def primary_node(harness: Harness) -> Node:
    node = current_primary(harness)
    if node is None:
        raise HarnessError("no live primary found")
    return node


def last_assigned_replica_endpoint(primary: Node) -> str | None:
    found = None
    for line in process_log(primary).splitlines():
        match = re.search(r"assigned replica endpoint=(\S+)", line)
        if match:
            found = match.group(1)
    return found


def node_for_replica_endpoint(harness: Harness, endpoint: str) -> Node | None:
    needle = f"replica={endpoint}"
    for node in live_nodes(harness):
        if needle in process_log(node):
            return node
    return None


def isolate_replica_with_state(harness: Harness, timeout: float) -> Node:
    if len(nodes_with_committed_state(harness)) < 2:
        wait_live_primary(harness, timeout)
        seed_wal_state(harness)
    with_state = wait_committed_state_quorum(harness, 2, timeout)
    primary = primary_node(harness)
    replica = next((node for node in with_state if node is not primary), None)
    if replica is None:
        raise HarnessError("no replica with STATE")
    for node in list(live_nodes(harness)):
        if node is not primary and node is not replica:
            log(f"SIGKILL {node.name} so primary assigns {replica.name}")
            stop_node(node, kill=True)
    deadline = time.time() + timeout
    while time.time() < deadline:
        endpoint = last_assigned_replica_endpoint(primary)
        if endpoint and node_for_replica_endpoint(harness, endpoint) is replica:
            return replica
        time.sleep(0.5)
    raise HarnessError(f"primary did not assign {replica.name} with STATE")


def replica_node(harness: Harness) -> Node:
    primary = primary_node(harness)
    assigned = [
        node
        for node in live_nodes(harness)
        if node is not primary
        and "assigned replica catch-up reached donor head" in process_log(node)
    ]
    if assigned:
        return assigned[-1]
    return wait_assigned_replica(harness, primary, 90)


def crash_primary_and_wait_unique(
    harness: Harness, failpoint: str, batch: str, ids: list[str], extra: set[str]
) -> list[str]:
    restart_dead_nodes(harness)
    primary = primary_node(harness)
    marks = {node.name: len(process_log(node)) for node in harness.nodes if node is not primary}
    arm_failpoint(primary, failpoint)
    copy_batch(harness, batch)
    wait_node_exit(primary, timeout=180)
    promoted = wait_promoted(harness, primary, PROMOTE_WAIT_SECONDS, since=marks)
    log(f"promoted {promoted.name} after {failpoint}")
    return wait_unique_ids(harness, set(ids) | extra, timeout=180)


def scenario_bootstrap(harness: Harness) -> None:
    log("scenario bootstrap_three_nodes")
    copy_batch(harness, "batch1.jsonl")
    for node in harness.nodes[:2]:
        start_node(harness, node)
    wait_membership(harness, ready=2, timeout=90)
    for node in harness.nodes[:2]:
        wait_for_log(node, "clustered scheduler started", 60)
    deadline = time.time() + 90
    while time.time() < deadline:
        if any("clustered primary ingest started" in process_log(node) for node in harness.nodes[:2]):
            return
        time.sleep(1)
    raise HarnessError("no node became clustered primary")


def scenario_ingest_and_query(harness: Harness) -> list[str]:
    log("scenario ingest_and_query_union")
    wait_parquet(harness.warehouse, timeout=180)
    expected = {"evt-1", "evt-2", "evt-3", "evt-4", "evt-5"}
    ids = wait_unique_ids(harness, expected, timeout=180)
    log(f"queried {len(ids)} rows from Iceberg UNION live WAL")
    return ids


def scenario_ballista_cluster_query(harness: Harness, expected: list[str]) -> None:
    log("scenario ballista_cluster_query")
    live = [node for node in harness.nodes[:2] if node.process is not None]
    if len(live) < 2:
        raise HarnessError("need two live ingesting nodes")
    elected = wait_same_elected_scheduler(live, timeout=60)
    log(f"shared elected_scheduler={elected}")
    ready = [
        item
        for item in wait_membership(harness, ready=2, timeout=30)
        if attr_bool(item, "ready")
    ]
    if len(ready) < 2:
        raise HarnessError(f"need two ready replicas, got {len(ready)}")
    expected_set = set(expected)
    flights: list[str] = []
    for node in live:
        flight_addr = last_flight_addr(node)
        if not flight_addr:
            raise HarnessError(f"{node.name} has no advertised flight_addr")
        flights.append(flight_addr)
    ready_flights = {attr_s(item, "flight_addr") for item in ready}
    missing = [addr for addr in flights if addr not in ready_flights]
    if missing:
        raise HarnessError(f"live flight_addr missing from ready membership: {missing}")
    for flight_addr in flights:
        ids = query_flight_ids(flight_addr, ROW_SQL)
        if set(ids) != expected_set or len(ids) != len(expected):
            raise HarnessError(
                f"Flight SQL on {flight_addr} ids={ids} expected={expected}"
            )
        count = query_flight_count(flight_addr, COUNT_SQL)
        if count != len(expected):
            raise HarnessError(
                f"Flight SQL on {flight_addr} count={count} expected={len(expected)}"
            )
        log(f"Ballista cluster query on {flight_addr} ids={ids} count={count}")


def scenario_query_every_replica(harness: Harness, expected: list[str]) -> None:
    log("scenario query_from_every_ready_replica")
    ready = wait_membership(harness, ready=2, timeout=30)
    ready_count = sum(1 for item in ready if item.get("ready", {"BOOL": False}).get("BOOL") is True)
    if ready_count < 2:
        raise HarnessError(f"need >=2 ready replicas, got {ready_count}")
    stdout = run_query(harness, ROW_SQL)
    ids = parse_ids(stdout)
    missing = [item for item in expected if item not in ids]
    if missing:
        raise HarnessError(
            f"ready-replica query missing {missing}: got {ids} expected {expected}\n{stdout}"
        )


def scenario_late_catchup(harness: Harness, expected: list[str]) -> None:
    log("scenario late_node_wal_catchup")
    late = harness.nodes[2]
    start_node(harness, late)
    wait_for_log(late, "clustered scheduler started", 60)
    # Spare publishes a membership row but is not query-ready until assigned.
    wait_membership(harness, ready=2, timeout=90, nodes=3)
    primary = primary_node(harness)
    replica = next(
        (
            node
            for node in harness.nodes[:2]
            if node is not primary
            and node.process is not None
            and node.process.poll() is None
        ),
        None,
    )
    if replica is None:
        raise HarnessError("no live ingest replica to SIGKILL for spare catch-up")
    log(f"SIGKILL replica {replica.name} so spare is assigned and catches up")
    stop_node(replica, kill=True)
    wait_for_log(late, "assigned replica catch-up reached donor head", 180)
    log(f"restart {replica.name} as spare so failover still has a replica candidate")
    start_node(harness, replica)
    wait_for_log(replica, "clustered scheduler started", 60)
    stdout = run_query(harness, ROW_SQL)
    ids = parse_ids(stdout)
    if ids != expected:
        raise HarnessError(
            f"query after late catch-up disagreed: got {ids} expected {expected}\n{stdout}"
        )


def scenario_failover(harness: Harness, before: list[str]) -> list[str]:
    log("scenario primary_kill_failover")
    primary = primary_node(harness)
    log(f"SIGKILL primary {primary.name} pid={primary.process.pid if primary.process else None}")
    stop_node(primary, kill=True)
    copy_batch(harness, "batch2.jsonl")
    survivors = [node for node in harness.nodes if node is not primary]
    deadline = time.time() + PROMOTE_WAIT_SECONDS
    promoted = None
    while time.time() < deadline:
        for node in survivors:
            if "clustered primary ingest started" in process_log(node):
                promoted = node
                break
        if promoted is not None:
            break
        time.sleep(1)
    if promoted is None:
        raise HarnessError(
            "no survivor became primary after lease steal window\n"
            + "\n".join(f"--- {n.name} ---\n{node_log(n)[-2000:]}" for n in survivors)
        )
    log(f"promoted {promoted.name}")
    deadline = time.time() + 180
    expected = set(before) | {"evt-6", "evt-7", "evt-8"}
    last = ""
    while time.time() < deadline:
        try:
            last = run_query(harness, ROW_SQL)
            ids = parse_ids(last)
            if set(ids) == expected and len(ids) == len(expected):
                return ids
        except HarnessError as err:
            last = str(err)
        time.sleep(3)
    raise HarnessError(f"failover query never reached {sorted(expected)}:\n{last}")


def scenario_sigkill_no_dupes(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario sigkill_during_ingest_no_duplicate_rows")
    restart_dead_nodes(harness)
    primary = primary_node(harness)
    arm_after_prepared(primary)
    copy_batch(harness, "batch3.jsonl")
    wait_node_exit(primary, timeout=180)
    survivors = [node for node in harness.nodes if node is not primary]
    deadline = time.time() + PROMOTE_WAIT_SECONDS
    promoted = None
    while time.time() < deadline:
        for node in survivors:
            if node.process is None or node.process.poll() is not None:
                continue
            if "clustered primary ingest started" in process_log(node):
                promoted = node
                break
        if promoted is not None:
            break
        time.sleep(1)
    if promoted is None:
        raise HarnessError(
            "no survivor became primary after after_prepared abort\n"
            + "\n".join(f"--- {n.name} ---\n{node_log(n)[-2000:]}" for n in survivors)
        )
    log(f"promoted {promoted.name} after prepared-window crash")
    expected = set(ids) | {"evt-9", "evt-10"}
    last = ""
    deadline = time.time() + 180
    while time.time() < deadline:
        try:
            last = run_query(harness, ROW_SQL)
            got = parse_ids(last)
            if set(got) == expected and len(got) == len(expected):
                count = parse_count(run_query(harness, COUNT_SQL))
                if count != len(expected):
                    raise HarnessError(f"count {count} != unique ids {len(expected)}")
                return got
        except HarnessError as err:
            last = str(err)
        time.sleep(3)
    raise HarnessError(f"prepared-crash query never reached {sorted(expected)}:\n{last}")


def scenario_after_local_commit(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario after_local_commit_no_dupes")
    restart_dead_nodes(harness)
    primary = primary_node(harness)
    marks = {node.name: len(process_log(node)) for node in harness.nodes if node is not primary}
    arm_failpoint(primary, "after_local_commit")
    copy_batch(harness, "batch4.jsonl")
    wait_node_exit(primary, timeout=180)
    promoted = wait_promoted(harness, primary, PROMOTE_WAIT_SECONDS, since=marks)
    log(f"promoted {promoted.name} after local-commit crash")
    expected = set(ids) | {"evt-11", "evt-12"}
    return wait_unique_ids(harness, expected, timeout=180)


def scenario_schema_evolution(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario schema_evolution")
    restart_dead_nodes(harness)
    stdout = run_query(harness, "SELECT id, region FROM hla_events ORDER BY id")
    if "evt-11" not in stdout or "evt-1" not in stdout:
        raise HarnessError(f"schema evolution UNION missing rows:\n{stdout}")
    got = parse_ids(run_query(harness, ROW_SQL))
    if set(got) != set(ids) or len(got) != len(ids):
        raise HarnessError(f"schema evolution double-counted: {got} vs {ids}")
    return got


def scenario_after_replica_ack(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario after_replica_ack_no_dupes")
    return crash_primary_and_wait_unique(
        harness, "after_replica_ack", "batch5.jsonl", ids, {"evt-13", "evt-14"}
    )


def scenario_after_offsets_published(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario after_offsets_published_no_dupes")
    return crash_primary_and_wait_unique(
        harness,
        "after_offsets_published",
        "batch6.jsonl",
        ids,
        {"evt-15", "evt-16"},
    )


def scenario_replica_after_prepared(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario replica_after_prepared")
    restart_dead_nodes(harness)
    replica = replica_node(harness)
    primary = primary_node(harness)
    arm_failpoint(replica, "replica_after_prepared")
    copy_batch(harness, "batch7.jsonl")
    wait_node_exit(replica, timeout=180)
    stop_node(primary, kill=True)
    start_node(harness, replica)
    wait_for_log(replica, "clustered scheduler started", 60)
    promoted = wait_promoted(harness, primary, PROMOTE_WAIT_SECONDS)
    log(f"promoted {promoted.name} after replica_after_prepared")
    return wait_unique_ids(harness, set(ids) | {"evt-17", "evt-18"}, timeout=180)


def scenario_after_compaction_sink(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario after_compaction_sink")
    restart_dead_nodes(harness)
    deadline = time.time() + 20
    last = parquet_files(harness.warehouse)
    while time.time() < deadline:
        time.sleep(2)
        now = parquet_files(harness.warehouse)
        if now == last:
            break
        last = now
    before = last
    primary = primary_node(harness)
    arm_failpoint(primary, "after_compaction_sink")
    copy_batch(harness, "batch8.jsonl")
    wait_node_exit(primary, timeout=180)
    after_crash = parquet_files(harness.warehouse)
    promoted = wait_promoted(harness, primary, PROMOTE_WAIT_SECONDS)
    log(f"promoted {promoted.name} after compaction-sink crash")
    got = wait_unique_ids(harness, set(ids) | {"evt-19", "evt-20"}, timeout=180)
    after_recovery = parquet_files(harness.warehouse)
    if len(after_recovery) > len(after_crash) + 2:
        raise HarnessError(
            "compaction replay wrote duplicate parquet: "
            f"before={len(before)} crash={len(after_crash)} recovery={len(after_recovery)}"
        )
    return got


def scenario_query_during_compaction_sent(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario query_during_compaction_sent")
    restart_dead_nodes(harness)
    deadline = time.time() + 20
    last = parquet_files(harness.warehouse)
    while time.time() < deadline:
        time.sleep(2)
        now = parquet_files(harness.warehouse)
        if now == last:
            break
        last = now
    primary = primary_node(harness)
    hold = arm_failpoint(primary, "hold_before_compaction_sink")
    copy_batch(harness, "batch11.jsonl")
    wait_for_log(primary, "clustered failpoint hold", 180)
    if not hold.exists():
        raise HarnessError("hold file vanished before Sent-window query")
    expected = set(ids) | {"evt-25", "evt-26"}
    last_err = ""
    query_deadline = time.time() + 180
    got: list[str] = []
    while time.time() < query_deadline:
        try:
            stdout = run_query(harness, ROW_SQL)
            got = parse_ids(stdout)
            last_err = stdout
            if set(got) == expected and len(got) == len(expected):
                if not hold.exists():
                    raise HarnessError(
                        "UNION reached new ids after hold file was released; "
                        "did not observe the Sent-before-sink window"
                    )
                break
        except HarnessError as err:
            last_err = str(err)
        time.sleep(2)
    else:
        raise HarnessError(
            f"query during Sent never reached {sorted(expected)}:\n{last_err}"
        )
    hold.unlink(missing_ok=True)
    return wait_unique_ids(harness, expected, timeout=120)


def scenario_snapshot_catchup_fourth_node(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario snapshot_catchup_fourth_node")
    restart_dead_nodes(harness)
    retain_deadline = time.time() + 180
    retained = None
    while time.time() < retain_deadline:
        for node in live_nodes(harness):
            for line in node_log(node).splitlines():
                if "clustered snapshot retained" in line and "live_segments=0" in line:
                    retained = node
                    break
            if retained is not None:
                break
        if retained is not None:
            break
        time.sleep(1)
    if retained is None:
        raise HarnessError(
            "no node retained a live snapshot after compaction/reclaim\n"
            + "\n".join(f"--- {n.name} ---\n{node_log(n)[-2000:]}" for n in live_nodes(harness))
        )
    primary = primary_node(harness)
    for node in list(live_nodes(harness)):
        if node.name == primary.name:
            continue
        log(f"SIGKILL {node.name} so empty n4 is the replacement replica")
        stop_node(node, kill=True)
    n4 = Node(
        name="n4",
        host="hla-host-4",
        data_dir=harness.workdir / "node4",
        log_path=harness.workdir / "logs" / "n4.log",
    )
    harness.nodes.append(n4)
    start_node(harness, n4)
    wait_for_log(n4, "clustered scheduler started", 60)
    install_deadline = time.time() + 180
    while time.time() < install_deadline:
        text = node_log(n4)
        if "clustered snapshot installed" in text or "assigned replica catch-up reached donor head" in text:
            break
        time.sleep(1)
    else:
        raise HarnessError(
            f"n4 did not install a snapshot or catch up:\n{node_log(n4)[-4000:]}"
        )
    if "clustered snapshot installed" not in node_log(n4):
        raise HarnessError(
            "n4 caught up without installing a snapshot pack; expected post-reclaim snapshot\n"
            + node_log(n4)[-4000:]
        )
    restart_dead_nodes(harness)
    return wait_unique_ids(harness, set(ids), timeout=180)


def scenario_sigstop_old_primary(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario sigstop_old_primary")
    # Compaction crash left a node dead. Quorum is 2, so steal needs a live
    # replica candidate besides the promoter. Restart the spare first.
    restart_dead_nodes(harness)
    deadline = time.time() + 60
    while time.time() < deadline:
        if len(live_nodes(harness)) >= 3:
            break
        time.sleep(1)
    else:
        raise HarnessError("need three live processes before SIGSTOP")
    time.sleep(12)
    primary = primary_node(harness)
    _replica = replica_node(harness)
    if primary.process is None:
        raise HarnessError("primary has no process")
    pid = primary.process.pid
    log(f"SIGSTOP primary {primary.name} pid={pid}")
    os.kill(pid, signal.SIGSTOP)
    try:
        promoted = wait_promoted(harness, primary, PROMOTE_WAIT_SECONDS)
        log(f"promoted {promoted.name} while old primary is stopped")
        copy_batch(harness, "batch9.jsonl")
        got = wait_unique_ids(harness, set(ids) | {"evt-21", "evt-22"}, timeout=180)
        os.kill(pid, signal.SIGCONT)
        time.sleep(8)
        late = parse_ids(run_query(harness, ROW_SQL))
        if set(late) != set(got) or len(late) != len(got):
            raise HarnessError(f"old primary resumed and duplicated rows: {late} vs {got}")
        return got
    finally:
        try:
            os.kill(pid, signal.SIGCONT)
        except OSError:
            pass
        stop_node(primary, kill=True)


def scenario_two_node_quorum_stall(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario two_node_quorum_stall")
    restart_dead_nodes(harness)
    primary = primary_node(harness)
    spares = [node for node in live_nodes(harness) if node is not primary]
    if len(spares) < 2:
        raise HarnessError("two-node stall needs a primary plus two other processes")
    for spare in spares:
        log(f"SIGKILL {spare.name} leaving only {primary.name}")
        stop_node(spare, kill=True)
    copy_batch(harness, "batch10.jsonl")
    time.sleep(20)
    try:
        stalled = parse_ids(run_query(harness, ROW_SQL))
    except HarnessError:
        stalled = list(ids)
    extra = {"evt-23", "evt-24"}
    if extra & set(stalled):
        raise HarnessError(
            f"writes continued without a replica quorum: {stalled}"
        )
    start_node(harness, spares[0])
    wait_for_log(spares[0], "clustered scheduler started", 60)
    wait_for_log(spares[0], "assigned replica catch-up reached donor head", 120)
    return wait_unique_ids(harness, set(ids) | extra, timeout=180)


def scenario_sigterm_drain(harness: Harness, ids: list[str]) -> None:
    log("scenario sigterm_drain")
    primary = primary_node(harness)
    stop_node(primary, kill=False)
    if primary.process is not None:
        raise HarnessError(f"{primary.name} did not exit after SIGTERM")
    got = wait_unique_ids(harness, set(ids), timeout=60)
    if set(got) != set(ids) or len(got) != len(ids):
        raise HarnessError(f"query after SIGTERM disagreed: {got} vs {ids}")


def scenario_truncated_log_restart(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario truncated_log_restart")
    restart_dead_nodes(harness)
    replica = replica_node(harness)
    stop_node(replica, kill=True)
    state = state_file_path(replica)
    log_path = mutation_log_path(replica)
    if not state.is_file():
        raise HarnessError(f"missing STATE at {state}")
    if not log_path.is_file():
        raise HarnessError(f"missing mutation.log at {log_path}")
    if log_path.stat().st_size >= 8:
        truncate_mutation_log_tail(log_path)
    bump_state_committed(state)
    start_node(harness, replica)
    deadline = time.time() + 60
    refused = False
    while time.time() < deadline:
        text = node_log(replica)
        if "STATE is ahead of mutation.log" in text or "refuse to apply" in text:
            refused = True
            break
        if replica.process is not None and replica.process.poll() is not None:
            if "STATE is ahead of mutation.log" in text or "refuse to apply" in text:
                refused = True
            break
        time.sleep(0.5)
    if not refused:
        raise HarnessError(
            f"{replica.name} did not refuse STATE/log mismatch:\n{node_log(replica)[-4000:]}"
        )
    stop_node(replica, kill=True)
    got = wait_unique_ids(harness, set(ids), timeout=60)
    if set(got) != set(ids) or len(got) != len(ids):
        raise HarnessError(f"query after truncated log disagreed: {got} vs {ids}")
    wipe_pipeline_data(replica)
    return ids


def scenario_enospc_during_prepared(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario enospc_during_prepared")
    restart_dead_nodes(harness)
    primary = primary_node(harness)
    arm_failpoint(primary, "prepared_disk_full")
    copy_batch(harness, "batch12.jsonl")
    wait_for_log(primary, "clustered failpoint disk full", 180)
    if primary.process is None or primary.process.poll() is not None:
        raise HarnessError("primary aborted on disk-full failpoint; expected fail-closed stay-up")
    time.sleep(3)
    got = parse_ids(run_query(harness, ROW_SQL))
    extra = {"evt-27", "evt-28"}
    if extra & set(got):
        raise HarnessError(f"disk-full Prepared still published {extra & set(got)}")
    if set(ids) - set(got):
        raise HarnessError(f"disk-full query dropped prior ids: {set(ids) - set(got)}")
    # Drop the source file first. Unlinking the failpoint while the batch is
    # still armed lets an in-flight retry publish evt-27/evt-28, which then
    # fails hash_conflict uniqueness. Keep prepared_disk_full until wipe.
    (harness.events_dir / "batch12.jsonl").unlink(missing_ok=True)
    return ids


def scenario_hash_conflict_no_majority(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario hash_conflict_no_majority")
    restart_dead_nodes(harness)
    targets = wait_state_quorum(harness, 3, 90)
    for node in list(harness.nodes):
        stop_node(node, kill=True)
    for i, node in enumerate(targets):
        state = state_file_path(node)
        if not state.is_file():
            raise HarnessError(f"missing STATE at {state}")
        flip_state_hash(state, 24 + i)
    for node in targets:
        start_node(harness, node)
        wait_for_log(node, "clustered scheduler started", 60)
    marks = {node.name: len(process_log(node)) for node in live_nodes(harness)}
    deadline = time.time() + LEASE_STEAL_SECONDS + 45
    while time.time() < deadline:
        for node in live_nodes(harness):
            added = process_log(node)[marks.get(node.name, 0) :]
            if "clustered primary ingest started" in added:
                raise HarnessError(
                    f"{node.name} promoted despite three disagreeing heads:\n{added[-2000:]}"
                )
        time.sleep(1)
    try:
        got = parse_ids(run_query(harness, ROW_SQL))
        if set(got) != set(ids) or len(got) != len(ids):
            raise HarnessError(
                f"hash-conflict Iceberg-only query disagreed: {got} vs {ids}"
            )
        log("hash-conflict query served prior ids while heads disagreed")
    except HarnessError as err:
        if "hash-conflict Iceberg-only query disagreed" in str(err):
            raise
        log("hash-conflict query failed closed while heads disagreed")
    for node in harness.nodes:
        stop_node(node, kill=True)
        wipe_pipeline_data(node)
        start_node(harness, node)
        wait_for_log(node, "clustered scheduler started", 60)
    deadline = time.time() + PROMOTE_WAIT_SECONDS
    promoted = None
    while time.time() < deadline:
        for node in live_nodes(harness):
            if "clustered primary ingest started" in process_log(node):
                promoted = node
                break
        if promoted is not None:
            break
        time.sleep(1)
    if promoted is None:
        raise HarnessError("no primary after wiping diverged copies")
    log(f"promoted {promoted.name} after wiping diverged copies")
    return wait_unique_ids(harness, set(ids), timeout=180)


def scenario_restart_generation(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario restart_generation")
    restart_dead_nodes(harness)
    replica = replica_node(harness)
    before = [
        item
        for item in membership_rows(harness)
        if attr_s(item, "host_id") == replica.host
    ]
    if not before:
        raise HarnessError(f"no membership row for host {replica.host}")
    old_sk = attr_s(before[0], "SK")
    stop_node(replica, kill=True)
    start_node(harness, replica)
    wait_for_log(replica, "clustered scheduler started", 60)
    deadline = time.time() + 60
    while time.time() < deadline:
        rows = [
            item
            for item in membership_rows(harness)
            if attr_s(item, "host_id") == replica.host
        ]
        new_sks = {attr_s(item, "SK") for item in rows}
        if new_sks and old_sk not in new_sks:
            log(f"host {replica.host} membership SK {old_sk} -> {sorted(new_sks)}")
            got = parse_ids(run_query(harness, ROW_SQL))
            if set(got) != set(ids):
                raise HarnessError(f"query after restart generation disagreed: {got} vs {ids}")
            return ids
        time.sleep(1)
    raise HarnessError(
        f"restarted {replica.name} kept membership SK {old_sk}"
    )


def scenario_donor_kill_mid_catchup(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario donor_kill_mid_catchup")
    restart_dead_nodes(harness)
    catching = replica_node(harness)
    donor = primary_node(harness)
    spares = [node for node in live_nodes(harness) if node is not catching and node is not donor]
    for spare in spares:
        stop_node(spare, kill=True)
    stop_node(catching, kill=True)
    wipe_pipeline_data(catching)
    start_node(harness, catching)
    wait_for_log(catching, "clustered scheduler started", 60)
    wait_for_log(catching, "assigned replica catch-up started", 120)
    donor = primary_node(harness)
    if donor.process is None:
        raise HarnessError("donor has no process")
    os.kill(donor.process.pid, signal.SIGSTOP)
    time.sleep(1)
    os.kill(donor.process.pid, signal.SIGKILL)
    donor.process = None
    if spares:
        start_node(harness, spares[0])
        wait_for_log(spares[0], "clustered scheduler started", 60)
    promoted = wait_promoted(harness, donor, PROMOTE_WAIT_SECONDS)
    log(f"promoted {promoted.name} after donor kill mid catch-up")
    wait_for_log(catching, "assigned replica catch-up reached donor head", 180)
    return wait_unique_ids(harness, set(ids), timeout=180)


def scenario_corrupt_replica_purge(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario corrupt_replica_purge")
    restart_dead_nodes(harness)
    replica = isolate_replica_with_state(harness, 90)
    stop_node(replica, kill=True)
    corrupt_diverged_snapshot_head(replica)
    start_node(harness, replica)
    wait_for_log(replica, "clustered scheduler started", 60)
    copy_batch(harness, "batch12.jsonl")
    wait_for_log(replica, "purged replica pipeline", 180)
    wait_for_log(replica, "assigned replica catch-up reached donor head", 180)
    (harness.events_dir / "purge-seed.jsonl").unlink(missing_ok=True)
    got = wait_unique_ids(
        harness,
        set(ids) | {"evt-27", "evt-28"},
        timeout=180,
        allowed_extra={"evt-purge-seed"},
    )
    wipe_pipeline_data(replica)
    stop_node(replica, kill=True)
    start_node(harness, replica)
    wait_for_log(replica, "assigned replica catch-up reached donor head", 180)
    return got


def scenario_before_compaction_sink_replay(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario before_compaction_sink_replay")
    restart_dead_nodes(harness)
    deadline = time.time() + 20
    last = parquet_files(harness.warehouse)
    while time.time() < deadline:
        time.sleep(2)
        now = parquet_files(harness.warehouse)
        if now == last:
            break
        last = now
    before = last
    primary = primary_node(harness)
    arm_failpoint(primary, "before_compaction_sink")
    copy_batch(harness, "batch14.jsonl")
    wait_node_exit(primary, timeout=180)
    after_crash = parquet_files(harness.warehouse)
    if len(after_crash) > len(before) + 2:
        raise HarnessError(
            "before_compaction_sink wrote parquet before abort: "
            f"before={len(before)} crash={len(after_crash)}"
        )
    promoted = wait_promoted(harness, primary, PROMOTE_WAIT_SECONDS)
    log(f"promoted {promoted.name} after before-sink crash")
    got = wait_unique_ids(harness, set(ids) | {"evt-31", "evt-32"}, timeout=180)
    after_recovery = parquet_files(harness.warehouse)
    if len(after_recovery) > len(after_crash) + 4:
        raise HarnessError(
            "compaction replay wrote duplicate parquet: "
            f"crash={len(after_crash)} recovery={len(after_recovery)}"
        )
    return got


def scenario_query_each_replica_socket(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario query_each_replica_socket")
    restart_dead_nodes(harness)
    ready = [
        item
        for item in wait_membership(harness, ready=2, timeout=60)
        if attr_bool(item, "ready")
    ]
    if len(ready) < 2:
        raise HarnessError(f"need two ready replicas, got {len(ready)}")
    hello = encode_client_hello(
        cluster_id=CLUSTER_ID, protocol_min=CURRENT_PROTOCOL, protocol_max=CURRENT_PROTOCOL
    )
    status_req = encode_status_request()
    flights: list[str] = []
    ctx = replica_ssl_context(harness.tls_dir)
    for item in ready:
        replica_addr = attr_s(item, "replica_addr")
        flight_addr = attr_s(item, "flight_addr")
        if not replica_addr or not flight_addr:
            raise HarnessError(f"membership row missing addrs: {item}")
        host, port_s = replica_addr.rsplit(":", 1)
        with socket.create_connection((host, int(port_s)), timeout=5) as raw:
            with ctx.wrap_socket(raw, server_hostname="skippr-cluster") as sock:
                sock.settimeout(5)
                sock.sendall(struct.pack("<I", len(hello)) + hello)
                hdr = recvall(sock, 4)
                if hdr is None:
                    raise HarnessError(f"no HelloOk from {replica_addr}")
                (size,) = struct.unpack("<I", hdr)
                reply = recvall(sock, size)
                if reply is None or frame_field(reply) != 2:
                    raise HarnessError(f"expected HelloOk from {replica_addr}")
                sock.sendall(struct.pack("<I", len(status_req)) + status_req)
                hdr = recvall(sock, 4)
                if hdr is None:
                    raise HarnessError(f"no StatusOk from {replica_addr}")
                (size,) = struct.unpack("<I", hdr)
                status = recvall(sock, size)
                if status is None:
                    raise HarnessError(f"empty StatusOk from {replica_addr}")
        flights.append(flight_addr)
    per_socket: list[list[str]] = []
    for flight_addr in flights:
        got = query_flight_ids(flight_addr, ROW_SQL)
        per_socket.append(got)
        log(f"Flight SQL query on {flight_addr} ids={got}")
    return ids


def scenario_query_retry_lagging(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario query_retry_lagging")
    restart_dead_nodes(harness)
    replica = replica_node(harness)
    if replica.process is None:
        raise HarnessError("replica has no process")
    pid = replica.process.pid
    log(f"SIGSTOP lagging replica {replica.name} pid={pid}")
    os.kill(pid, signal.SIGSTOP)
    try:
        copy_batch(harness, "batch13.jsonl")
        expected = set(ids) | {"evt-29", "evt-30"}
        wait_unique_ids(harness, expected, timeout=180)
        got = parse_ids(run_query(harness, ROW_SQL))
        if not got:
            raise HarnessError("query after SIGSTOP returned no ids")
        log(f"query after SIGSTOP one replica ids={got}")
        stopped = [node for node in live_nodes(harness) if node is not replica]
        for node in stopped:
            if node.process is None:
                continue
            os.kill(node.process.pid, signal.SIGSTOP)
        try:
            iceberg_only = run_query(harness, ROW_SQL)
            iceberg_ids = parse_ids(iceberg_only)
            log(f"Iceberg-only query after all SIGSTOP ids={iceberg_ids}")
        except HarnessError as err:
            raise HarnessError(f"Iceberg-only query failed with no query socket: {err}") from err
        finally:
            for node in stopped:
                if node.process is not None:
                    os.kill(node.process.pid, signal.SIGCONT)
    finally:
        os.kill(pid, signal.SIGCONT)
    return wait_unique_ids(harness, set(ids) | {"evt-29", "evt-30"}, timeout=180)


def scenario_failed_drain_holds_lease(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario failed_drain_holds_lease")
    restart_dead_nodes(harness)
    primary = primary_node(harness)
    rows = [
        item
        for item in membership_rows(harness)
        if attr_s(item, "host_id") == primary.host
    ]
    if not rows:
        raise HarnessError("primary missing membership row")
    owner_sk = attr_s(rows[0], "SK")
    owner_uuid = owner_sk.removeprefix("node#")
    if primary.process is None:
        raise HarnessError("primary has no process")
    pid = primary.process.pid
    os.kill(pid, signal.SIGTERM)
    os.kill(pid, signal.SIGKILL)
    try:
        primary.process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    primary.process = None
    time.sleep(1)
    item = lease_item(harness)
    if item is None:
        raise HarnessError("lease item missing after failed drain")
    if attr_bool(item, "released"):
        raise HarnessError("failed drain released the lease")
    owner = attr_s(item, "owner_node")
    if owner and owner != owner_uuid:
        raise HarnessError(f"lease owner jumped during failed drain: {owner} vs {owner_uuid}")
    got = parse_ids(run_query(harness, ROW_SQL))
    if set(got) != set(ids):
        raise HarnessError(f"query after failed drain disagreed: {got} vs {ids}")
    return ids


def write_two_pipeline_yml(
    path: Path, events_a: Path, events_b: Path, warehouse: Path
) -> None:
    warehouse_uri = f"file://{warehouse}"
    path.write_text(
        f"""skippr:
  workspace: {WORKSPACE}
  skippr_s3_bucket: skippr-hla-e2e-unused
  skipprd_el_storage_mode: local

pipelines:
  {PIPELINE}:
    auto_approve: yes
    env: test
    buffer_threshold_bytes: 256
    buffer_threshold_seconds: 1
    data_source: data_sources.local_events
    data_sink: data_sinks.iceberg_local
    schema_sink: schema_sinks.iceberg_local
  hla_events_b:
    auto_approve: yes
    env: test
    buffer_threshold_bytes: 256
    buffer_threshold_seconds: 1
    data_source: data_sources.local_events_b
    data_sink: data_sinks.iceberg_b
    schema_sink: schema_sinks.iceberg_b

data_sources:
  local_events:
    File:
      path: {events_a}
      format: json
      batch_size_bytes: 1024
  local_events_b:
    File:
      path: {events_b}
      format: json
      batch_size_bytes: 1024

data_sinks:
  iceberg_local:
    Iceberg:
      table_namespace: hla
      table_prefix: hla
      table_location_prefix: {warehouse_uri}
      catalog:
        type: skippr
        table: {CATALOG_TABLE}
        warehouse: {warehouse_uri}
        region: us-east-1
    schema_sink: schema_sinks.iceberg_local
  iceberg_b:
    Iceberg:
      table_namespace: hla
      table_prefix: hla-b
      table_location_prefix: {warehouse_uri}
      catalog:
        type: skippr
        table: {CATALOG_TABLE}
        warehouse: {warehouse_uri}
        region: us-east-1
    schema_sink: schema_sinks.iceberg_b

schema_sinks:
  iceberg_local:
    Iceberg:
      table_namespace: hla
      table_prefix: hla
      table_location_prefix: {warehouse_uri}
      catalog:
        type: skippr
        table: {CATALOG_TABLE}
        warehouse: {warehouse_uri}
        region: us-east-1
  iceberg_b:
    Iceberg:
      table_namespace: hla
      table_prefix: hla-b
      table_location_prefix: {warehouse_uri}
      catalog:
        type: skippr
        table: {CATALOG_TABLE}
        warehouse: {warehouse_uri}
        region: us-east-1
""",
        encoding="utf-8",
    )


def scenario_two_pipelines(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario two_pipelines")
    for node in harness.nodes:
        stop_node(node, kill=True)
    events_b = harness.workdir / "events-b"
    events_b.mkdir(exist_ok=True)
    shutil.copyfile(TESTDATA / "batch15.jsonl", events_b / "batch15.jsonl")
    config = harness.workdir / "two-pipeline.yml"
    write_two_pipeline_yml(config, harness.events_dir, events_b, harness.warehouse)
    twins = [
        Node(
            name="pipe-a",
            host="hla-pipe-1",
            data_dir=harness.workdir / "pipe-a",
            log_path=harness.workdir / "logs" / "pipe-a.log",
        ),
        Node(
            name="pipe-b",
            host="hla-pipe-2",
            data_dir=harness.workdir / "pipe-b",
            log_path=harness.workdir / "logs" / "pipe-b.log",
        ),
    ]
    for node in twins:
        start_node(harness, node, config_path=config, pipeline=None)
        wait_for_log(node, "clustered scheduler started", 60)
    deadline = time.time() + 180
    saw_a = False
    saw_b = False
    while time.time() < deadline:
        combined = "\n".join(node_log(node) for node in twins)
        saw_a = "pipeline=hla_events" in combined and "clustered primary ingest started" in combined
        saw_b = "pipeline=hla_events_b" in combined
        if saw_a and saw_b:
            break
        time.sleep(1)
    else:
        raise HarnessError("two-pipeline cluster did not start primaries for both pipelines")
    for node in twins:
        pipelines = primary_ingest_pipelines(process_log(node))
        if len(pipelines) > 1:
            raise HarnessError(
                f"{node.name} became ActivePrimary for multiple pipelines: "
                f"{sorted(pipelines)}\n{process_log(node)[-2000:]}"
            )
    deadline = time.time() + 180
    got: list[str] = []
    last_err: HarnessError | None = None
    while time.time() < deadline:
        try:
            got = parse_ids(run_query(harness, ROW_SQL, config_path=config))
        except HarnessError as err:
            last_err = err
            time.sleep(2)
            continue
        if not (set(ids) - set(got)):
            break
        time.sleep(2)
    else:
        missing = set(ids) - set(got)
        detail = f"missing {missing}" if got else str(last_err)
        raise HarnessError(f"two-pipeline query dropped hla_events ids: {detail}")
    deadline = time.time() + 180
    got_b: list[str] = []
    while time.time() < deadline:
        try:
            got_b = parse_ids(
                run_query(harness, "SELECT id FROM hla_events_b ORDER BY id", config_path=config)
            )
        except HarnessError:
            time.sleep(2)
            continue
        if {"evt-b1", "evt-b2"} <= set(got_b):
            break
        time.sleep(2)
    else:
        raise HarnessError(f"two-pipeline query missed hla_events_b ids: {got_b}")
    replica_logs = "\n".join(node_log(node) for node in twins)
    if "assigned replica catch-up" not in replica_logs:
        raise HarnessError(
            "two-pipeline cluster never assigned a replica for the non-primary pipeline"
        )
    for node in twins:
        stop_node(node, kill=True)
    restart_dead_nodes(harness)
    return ids


def scenario_mixed_protocol_handshake(harness: Harness) -> None:
    log("scenario mixed_protocol_handshake")
    restart_dead_nodes(harness)
    addr = first_replica_addr(harness)
    ok = replica_hello(
        addr,
        encode_client_hello(
            cluster_id=CLUSTER_ID,
            protocol_min=CURRENT_PROTOCOL,
            protocol_max=CURRENT_PROTOCOL,
        ),
        harness.tls_dir,
    )
    if ok is None or frame_field(ok) != 2:
        raise HarnessError(f"current protocol Hello was not HelloOk from {addr}")
    rejected = replica_hello(
        addr,
        encode_client_hello(
            cluster_id=CLUSTER_ID, protocol_min=99, protocol_max=99
        ),
        harness.tls_dir,
    )
    if rejected is not None and frame_field(rejected) == 2:
        raise HarnessError("incompatible protocol range was accepted")
    live = live_nodes(harness)
    if live and "protocol range does not overlap" not in node_log(live[0]) and all(
        "protocol range does not overlap" not in node_log(node) for node in live
    ):
        log("incompatible Hello closed without HelloOk (server warn may be on another node)")
    else:
        log("incompatible protocol handshake rejected")


PROTOCOL_DUMMY_ADDR = "203.0.113.9:1948"
PROTOCOL_DUMMY_NODE = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"


def put_protocol3_dummy(harness: Harness, heartbeat: int) -> None:
    pk = f"cluster#{CLUSTER_ID}"
    harness.ddb.put_item(
        TableName=OFFSET_TABLE,
        Item={
            "PK": {"S": pk},
            "SK": {"S": f"node#{PROTOCOL_DUMMY_NODE}"},
            "host_label": {"S": "hla-host-protocol3"},
            "host_id": {"S": "hla-host-protocol3"},
            "replica_addr": {"S": PROTOCOL_DUMMY_ADDR},
            "flight_addr": {"S": "203.0.113.9:1949"},
            "gossip_addr": {"S": "203.0.113.9:1950"},
            "heartbeat": {"N": str(heartbeat)},
            "protocol_min": {"N": "3"},
            "protocol_max": {"N": "3"},
            "capabilities": {"S": ""},
            "ready": {"BOOL": True},
            "disk_pressure": {"BOOL": False},
        },
    )


def scenario_rolling_protocol_replacement(harness: Harness) -> None:
    log("scenario rolling_protocol_replacement")
    core = pin_core_cluster(harness)
    primary = wait_live_primary(harness, 90)
    assigned = wait_assigned_replica(harness, primary, 90)
    spares = [node for node in core if node is not assigned and node is not primary]
    if not spares:
        raise HarnessError("rolling protocol replacement needs a protocol-2 spare")
    spare = spares[0]
    mark = len(process_log(spare))
    heartbeat = 1
    put_protocol3_dummy(harness, heartbeat)
    log(f"SIGKILL assigned replica {assigned.name} so primary reassigns")
    stop_node(assigned, kill=True)
    deadline = time.time() + 120
    while time.time() < deadline:
        heartbeat += 1
        put_protocol3_dummy(harness, heartbeat)
        combined = "\n".join(node_log(node) for node in live_nodes(harness))
        if any(
            PROTOCOL_DUMMY_ADDR in line and "assigned replica" in line
            for line in combined.splitlines()
        ):
            raise HarnessError(f"protocol-3 dummy {PROTOCOL_DUMMY_ADDR} was assigned")
        if "assigned replica catch-up" in process_log(spare)[mark:]:
            break
        time.sleep(1)
    else:
        raise HarnessError(
            f"protocol-2 spare {spare.name} was not assigned after replica kill"
        )
    combined = "\n".join(node_log(node) for node in harness.nodes)
    if any(
        PROTOCOL_DUMMY_ADDR in line and "assigned replica" in line
        for line in combined.splitlines()
    ):
        raise HarnessError(f"protocol-3 dummy {PROTOCOL_DUMMY_ADDR} was assigned")
    start_node(harness, assigned)
    wait_for_log(assigned, "clustered scheduler started", 60)


def scenario_cold_start_membership(harness: Harness, ids: list[str]) -> list[str]:
    log("scenario cold_start_membership")
    for node in harness.nodes:
        stop_node(node, kill=True)
    n1, n2 = harness.nodes[0], harness.nodes[1]
    start_node(harness, n1)
    wait_for_log(n1, "clustered scheduler started", 60)
    wait_membership(harness, ready=1, timeout=90)
    start_node(harness, n2)
    wait_for_log(n2, "cold gossip seeds from membership", 60)
    wait_for_log(n2, "clustered scheduler started", 60)
    deadline = time.time() + 90
    while time.time() < deadline:
        if any(
            "clustered primary ingest started" in process_log(node) for node in (n1, n2)
        ):
            break
        time.sleep(1)
    else:
        raise HarnessError("cold-start pair formed no primary")
    restart_dead_nodes(harness)
    return wait_unique_ids(harness, set(ids), timeout=180)


def scenario_cross_cluster_handshake(harness: Harness) -> None:
    log("scenario cross_cluster_handshake")
    restart_dead_nodes(harness)
    addr = first_replica_addr(harness)
    reply = replica_hello(
        addr,
        encode_client_hello(
            cluster_id="other-lake",
            protocol_min=CURRENT_PROTOCOL,
            protocol_max=CURRENT_PROTOCOL,
        ),
        harness.tls_dir,
    )
    if reply is not None and frame_field(reply) == 2:
        raise HarnessError("cross-cluster Hello was accepted")
    if not any(
        "cluster_hash does not match" in node_log(node)
        for node in live_nodes(harness)
    ):
        log("cross-cluster Hello closed without HelloOk")
    else:
        log("cross-cluster handshake rejected")


def scenario_same_host_exclusion(harness: Harness) -> None:
    log("scenario same_host_exclusion")
    for node in harness.nodes:
        stop_node(node, kill=True)
    twin_a = Node(
        name="same-a",
        host="hla-same-host",
        data_dir=harness.workdir / "same-a",
        log_path=harness.workdir / "logs" / "same-a.log",
    )
    twin_b = Node(
        name="same-b",
        host="hla-same-host",
        data_dir=harness.workdir / "same-b",
        log_path=harness.workdir / "logs" / "same-b.log",
    )
    start_node(harness, twin_a)
    start_node(harness, twin_b)
    wait_for_log(twin_a, "clustered scheduler started", 60)
    wait_for_log(twin_b, "clustered scheduler started", 60)
    time.sleep(8)
    primaries = [
        node
        for node in (twin_a, twin_b)
        if "clustered primary ingest started" in process_log(node)
    ]
    replica_assigns = "AssignReplica" in node_log(twin_a) and "AssignReplica" in node_log(
        twin_b
    )
    stop_node(twin_a, kill=True)
    stop_node(twin_b, kill=True)
    if replica_assigns:
        raise HarnessError("same HostId processes assigned each other as replicas")
    if len(primaries) > 1:
        raise HarnessError("same HostId processes both became primary")
    log("same-host processes did not form primary+replica")


def scenario_at_least_once_reject(harness: Harness) -> None:
    log("scenario clustered_rejects_at_least_once")
    config = harness.workdir / "stdout.yml"
    config.write_text(
        f"""skippr:
  workspace: {WORKSPACE}
  skippr_s3_bucket: skippr-hla-e2e-unused
  skipprd_el_storage_mode: local

pipelines:
  {PIPELINE}:
    auto_approve: yes
    env: test
    data_source: data_sources.local_events
    data_sink: data_sinks.stdout_sink

data_sources:
  local_events:
    File:
      path: {harness.events_dir}
      format: json

data_sinks:
  stdout_sink:
    Stdout: {{}}
""",
        encoding="utf-8",
    )
    env = common_env(harness)
    env["DATA_DIR"] = str(harness.workdir / "stdout-node")
    Path(env["DATA_DIR"]).mkdir(parents=True, exist_ok=True)
    env["KUBERNETES_NODE_NAME"] = "hla-stdout"
    env["SKIPPR_CONFIG_FILE"] = str(config)
    result = subprocess.run(
        [
            str(harness.skipprd),
            "--config",
            str(config),
            "--wal-storage",
            "clustered",
            "--log",
            "info",
            "sync",
            "--pipeline",
            PIPELINE,
            "--output",
            "text",
        ],
        cwd=REPO_ROOT,
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    text = result.stdout + result.stderr
    if result.returncode == 0:
        raise HarnessError(f"clustered Stdout sink was accepted:\n{text}")
    if "ClusteredSinkNotIdempotent" not in text and "idempotent" not in text.lower():
        raise HarnessError(f"expected clustered sink rejection:\n{text}")
    log("clustered AtLeastOnce/Stdout sink rejected")


def reap_stale_hla_processes() -> None:
    subprocess.run(
        ["pkill", "-9", "-f", "/tmp/skippr-hla-e2e"],
        check=False,
        capture_output=True,
    )


def run_rolling_protocol_repro(harness: Harness) -> None:
    """Reproduce the 4-node leftover from snapshot_catchup, then pin n1–n3."""
    scenario_bootstrap(harness)
    scenario_ingest_and_query(harness)
    n4 = Node(
        name="n4",
        host="hla-host-4",
        data_dir=harness.workdir / "node4",
        log_path=harness.workdir / "logs" / "n4.log",
    )
    harness.nodes.append(n4)
    start_node(harness, n4)
    wait_for_log(n4, "clustered scheduler started", 60)
    scenario_rolling_protocol_replacement(harness)
    log("rolling_protocol_replacement passed")


def run(keep: bool, only: str | None = None) -> None:
    reap_stale_hla_processes()
    workdir = Path("/tmp/skippr-hla-e2e")
    if workdir.exists():
        shutil.rmtree(workdir)
    events_dir = workdir / "events"
    warehouse = workdir / "warehouse"
    events_dir.mkdir(parents=True)
    warehouse.mkdir(parents=True)
    config_path = workdir / "skippr.yml"
    write_skippr_yml(config_path, events_dir, warehouse)
    skipprd = build_skipprd()
    manifest_dir = stage_plugins(config_path)
    ddb = start_dynamodb()
    tls_dir = mint_cluster_tls(workdir)
    harness = Harness(
        workdir=workdir,
        skipprd=skipprd,
        config_path=config_path,
        warehouse=warehouse,
        events_dir=events_dir,
        manifest_dir=manifest_dir,
        keep=keep,
        tls_dir=tls_dir,
        ddb=ddb,
    )
    harness.nodes = [
        Node(
            name=f"n{idx}",
            host=f"hla-host-{idx}",
            data_dir=workdir / f"node{idx}",
            log_path=workdir / "logs" / f"n{idx}.log",
        )
        for idx in (1, 2, 3)
    ]
    try:
        if only == "rolling_protocol_replacement":
            run_rolling_protocol_repro(harness)
            return
        scenario_bootstrap(harness)
        ids = scenario_ingest_and_query(harness)
        scenario_ballista_cluster_query(harness, ids)
        assert_dynamo_closed(harness)
        scenario_query_every_replica(harness, ids)
        scenario_late_catchup(harness, ids)
        ids = scenario_failover(harness, ids)
        ids = scenario_sigkill_no_dupes(harness, ids)
        ids = scenario_after_local_commit(harness, ids)
        ids = scenario_schema_evolution(harness, ids)
        ids = scenario_after_replica_ack(harness, ids)
        ids = scenario_after_offsets_published(harness, ids)
        ids = scenario_replica_after_prepared(harness, ids)
        ids = scenario_after_compaction_sink(harness, ids)
        ids = scenario_query_during_compaction_sent(harness, ids)
        ids = scenario_snapshot_catchup_fourth_node(harness, ids)
        ids = scenario_sigstop_old_primary(harness, ids)
        ids = scenario_two_node_quorum_stall(harness, ids)
        scenario_sigterm_drain(harness, ids)
        ids = scenario_truncated_log_restart(harness, ids)
        ids = scenario_enospc_during_prepared(harness, ids)
        ids = scenario_hash_conflict_no_majority(harness, ids)
        ids = scenario_restart_generation(harness, ids)
        ids = scenario_donor_kill_mid_catchup(harness, ids)
        ids = scenario_corrupt_replica_purge(harness, ids)
        ids = scenario_before_compaction_sink_replay(harness, ids)
        ids = scenario_query_each_replica_socket(harness, ids)
        ids = scenario_query_retry_lagging(harness, ids)
        ids = scenario_failed_drain_holds_lease(harness, ids)
        ids = scenario_two_pipelines(harness, ids)
        scenario_mixed_protocol_handshake(harness)
        scenario_rolling_protocol_replacement(harness)
        ids = scenario_cold_start_membership(harness, ids)
        scenario_cross_cluster_handshake(harness)
        scenario_same_host_exclusion(harness)
        scenario_at_least_once_reject(harness)
        log("all scenarios passed: " + ", ".join(SCENARIOS))
    except Exception:
        for node in harness.nodes:
            log(f"--- {node.name} log ---\n{node_log(node)[-4000:]}")
        raise
    finally:
        for node in harness.nodes:
            stop_node(node, kill=True)
        if not keep:
            stop_dynamodb()
            shutil.rmtree(workdir, ignore_errors=True)
        else:
            log(f"kept workdir {workdir} and DynamoDB Local")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--keep", action="store_true", help="leave DynamoDB Local and workdir")
    parser.add_argument("--list", action="store_true", help="print scenario names")
    parser.add_argument(
        "--only",
        choices=["rolling_protocol_replacement"],
        help="run rolling_protocol_replacement against a 4-node leftover (n4)",
    )
    args = parser.parse_args()
    if args.list:
        for name in SCENARIOS:
            print(name)
        return 0
    try:
        run(keep=args.keep, only=args.only)
    except subprocess.CalledProcessError as err:
        log(f"command failed: {err}")
        return 1
    except HarnessError as err:
        log(str(err))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
