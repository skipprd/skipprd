#!/usr/bin/env python3
"""DynamoDB Streams LATEST iterator semantics (reproduces GitHub CDC follow race)."""

from __future__ import annotations

import os
import subprocess
import sys
import time
import unittest
from pathlib import Path

HARNESS_DIR = Path(__file__).resolve().parent
REPO_ROOT = HARNESS_DIR.parents[1]
sys.path.insert(0, str(HARNESS_DIR))


PORT = 18031
ENDPOINT = f"http://127.0.0.1:{PORT}"
TABLE = "skippr_ddb_latest_probe"
CONTAINER = "skippr-ddb-latest-probe"


def ddb_client():
    import boto3
    from botocore.config import Config as BotoConfig

    return boto3.client(
        "dynamodb",
        endpoint_url=ENDPOINT,
        region_name="us-east-1",
        aws_access_key_id="local",
        aws_secret_access_key="local",
        config=BotoConfig(retries={"max_attempts": 8}),
    )


def ensure_table() -> str:
    client = ddb_client()
    try:
        client.delete_table(TableName=TABLE)
        client.get_waiter("table_not_exists").wait(TableName=TABLE)
    except Exception:
        pass
    client.create_table(
        TableName=TABLE,
        AttributeDefinitions=[{"AttributeName": "id", "AttributeType": "S"}],
        KeySchema=[{"AttributeName": "id", "KeyType": "HASH"}],
        BillingMode="PAY_PER_REQUEST",
        StreamSpecification={
            "StreamEnabled": True,
            "StreamViewType": "NEW_AND_OLD_IMAGES",
        },
    )
    client.get_waiter("table_exists").wait(TableName=TABLE)
    desc = client.describe_table(TableName=TABLE)
    arn = desc["Table"]["LatestStreamArn"]
    assert arn
    return arn


def streams_client():
    return __import__("boto3").client(
        "dynamodbstreams",
        endpoint_url=ENDPOINT,
        region_name="us-east-1",
        aws_access_key_id="local",
        aws_secret_access_key="local",
    )


def shard_iterator_latest(stream_arn: str) -> str:
    streams = streams_client()
    desc = streams.describe_stream(StreamArn=stream_arn)
    shards = desc["StreamDescription"]["Shards"]
    if not shards:
        raise AssertionError("stream has no shards")
    shard_id = shards[0]["ShardId"]
    resp = streams.get_shard_iterator(
        StreamArn=stream_arn,
        ShardId=shard_id,
        ShardIteratorType="LATEST",
    )
    it = resp.get("ShardIterator")
    if not it:
        raise AssertionError("LATEST shard iterator missing")
    return it


def get_record_count(iterator: str) -> int:
    streams = streams_client()
    total = 0
    for _ in range(6):
        resp = streams.get_records(ShardIterator=iterator)
        recs = resp.get("Records") or []
        total += len(recs)
        iterator = resp.get("NextShardIterator") or iterator
        if recs:
            break
        time.sleep(0.3)
    return total


def docker_required() -> bool:
    try:
        subprocess.run(
            ["docker", "info"],
            capture_output=True,
            check=True,
            timeout=2,
        )
        return True
    except Exception:
        if os.environ.get("GITHUB_ACTIONS"):
            raise RuntimeError("Docker required on GitHub for DDB stream Latest tests")
        return False


@unittest.skipUnless(docker_required(), "Docker required for DynamoDB Local")
class DynamoDbStreamLatestTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        subprocess.run(["docker", "rm", "-f", CONTAINER], capture_output=True, check=False)
        subprocess.run(
            [
                "docker",
                "run",
                "-d",
                "--name",
                CONTAINER,
                "-p",
                f"{PORT}:8000",
                "amazon/dynamodb-local",
                "-jar",
                "DynamoDBLocal.jar",
                "-sharedDb",
                "-inMemory",
            ],
            check=True,
        )
        deadline = time.time() + 30
        while time.time() < deadline:
            try:
                ddb_client().list_tables()
                break
            except Exception:
                time.sleep(0.5)
        else:
            raise RuntimeError("DynamoDB Local did not become ready")

    @classmethod
    def tearDownClass(cls) -> None:
        subprocess.run(["docker", "rm", "-f", CONTAINER], capture_output=True, check=False)

    def test_latest_sees_puts_after_iterator(self) -> None:
        arn = ensure_table()
        it = shard_iterator_latest(arn)
        ddb_client().put_item(
            TableName=TABLE,
            Item={"id": {"S": "after"}, "name": {"S": "visible"}},
        )
        self.assertGreater(get_record_count(it), 0, "LATEST must see puts after iterator")

    def test_latest_drops_puts_before_iterator(self) -> None:
        arn = ensure_table()
        ddb_client().put_item(
            TableName=TABLE,
            Item={"id": {"S": "before"}, "name": {"S": "invisible"}},
        )
        time.sleep(0.2)
        it = shard_iterator_latest(arn)
        self.assertEqual(
            get_record_count(it),
            0,
            "LATEST must not replay puts that landed before the iterator (GitHub CDC race)",
        )


if __name__ == "__main__":
    if os.environ.get("GITHUB_ACTIONS") and not docker_required():
        raise SystemExit("Docker required on GitHub for DDB stream Latest tests")
    unittest.main()