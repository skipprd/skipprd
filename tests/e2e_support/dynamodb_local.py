"""DynamoDB Local for skipprd e2e harnesses."""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import tarfile
import time
import urllib.request
from pathlib import Path
from typing import Any

import boto3
from botocore.config import Config as BotoConfig
from botocore.exceptions import ClientError

REPO_ROOT = Path(__file__).resolve().parents[2]
DDB_LOCAL_DIR = REPO_ROOT / ".skippr" / "dynamodb-local"
DDB_LOCAL_URL = "https://d1ni2b6xgvw0s0.cloudfront.net/v2.x/dynamodb_local_latest.tar.gz"

_JAVA: dict[int, subprocess.Popen[str]] = {}


class DynamoDbLocalError(RuntimeError):
    pass


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
    raise DynamoDbLocalError("DynamoDB Local requires Java 17+")


def ensure_jar() -> Path:
    jar = DDB_LOCAL_DIR / "DynamoDBLocal.jar"
    if jar.is_file():
        return jar
    DDB_LOCAL_DIR.mkdir(parents=True, exist_ok=True)
    archive = DDB_LOCAL_DIR / "dynamodb_local_latest.tar.gz"
    urllib.request.urlretrieve(DDB_LOCAL_URL, archive)
    with tarfile.open(archive, "r:gz") as tar:
        tar.extractall(DDB_LOCAL_DIR)
    if not jar.is_file():
        raise DynamoDbLocalError(f"DynamoDB Local jar missing after extract: {jar}")
    return jar


def wait_client(endpoint: str) -> Any:
    client = boto3.client(
        "dynamodb",
        endpoint_url=endpoint,
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
    raise DynamoDbLocalError("DynamoDB Local did not become ready")


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


def start(
    *,
    port: int,
    tables: list[str],
    container: str,
) -> Any:
    stop(port=port, container=container)
    time.sleep(0.5)
    endpoint = f"http://127.0.0.1:{port}"
    docker = shutil.which("docker")
    if docker is not None:
        probe = subprocess.run([docker, "info"], capture_output=True, text=True)
        if probe.returncode == 0:
            subprocess.run([docker, "rm", "-f", container], check=False, capture_output=True)
            subprocess.run(
                [
                    docker,
                    "run",
                    "-d",
                    "--name",
                    container,
                    "-p",
                    f"{port}:8000",
                    "amazon/dynamodb-local",
                    "-jar",
                    "DynamoDBLocal.jar",
                    "-sharedDb",
                    "-inMemory",
                ],
                check=True,
            )
            client = wait_client(endpoint)
            for name in tables:
                ensure_pk_sk_table(client, name)
            return client
    jar = ensure_jar()
    log_path = DDB_LOCAL_DIR / f"local-{port}.log"
    log_file = log_path.open("w", encoding="utf-8")
    proc = subprocess.Popen(
        [
            java_bin(),
            f"-Djava.library.path={jar.parent / 'DynamoDBLocal_lib'}",
            "-jar",
            str(jar),
            "-sharedDb",
            "-inMemory",
            "-port",
            str(port),
        ],
        cwd=jar.parent,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        text=True,
    )
    _JAVA[port] = proc
    client = wait_client(endpoint)
    for name in tables:
        ensure_pk_sk_table(client, name)
    return client


def stop(*, port: int, container: str) -> None:
    proc = _JAVA.pop(port, None)
    if proc is not None and proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.send_signal(signal.SIGKILL)
            proc.wait(timeout=5)
    if shutil.which("docker"):
        subprocess.run(["docker", "rm", "-f", container], check=False, capture_output=True)
    lsof = shutil.which("lsof")
    if lsof:
        probe = subprocess.run(
            [lsof, "-nP", f"-iTCP:{port}", "-sTCP:LISTEN", "-t"],
            capture_output=True,
            text=True,
        )
        for pid in probe.stdout.split():
            try:
                os.kill(int(pid), signal.SIGTERM)
            except (ProcessLookupError, ValueError):
                pass
