#!/usr/bin/env python3

import re
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]

# These crates are connector-specific and must not leak back into the host
# dependency closure for the root `skippr` crate.
FORBIDDEN_PACKAGES = {
    "aws-sdk-dynamodbstreams",
    "aws-sdk-eventbridge",
    "aws-sdk-kinesis",
    "aws-sdk-redshiftdata",
    "aws-sdk-sns",
    "aws-sdk-sqs",
    "bb8",
    "bb8-tiberius",
    "clickhouse",
    "deltalake",
    "lapin",
    "mongodb",
    "mysql_async",
    "native-tls",
    "pgwire-replication",
    "postgres-native-tls",
    "rdkafka",
    "rumqttc",
    "ssh2",
    "tiberius",
    "tokio-postgres",
    "tokio-tungstenite",
    "tungstenite",
}


def main() -> int:
    result = subprocess.run(
        ["cargo", "tree", "-p", "skippr", "-e", "normal", "--prefix", "none"],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
        text=True,
    )

    leaked = set()
    for line in result.stdout.splitlines():
        match = re.match(r"^([A-Za-z0-9_.-]+) v", line.strip())
        if not match:
            continue
        package_name = match.group(1)
        if package_name in FORBIDDEN_PACKAGES:
            leaked.add(package_name)

    if leaked:
        print("host dependency boundary violated for root `skippr` crate:", file=sys.stderr)
        for name in sorted(leaked):
            print(f"  - {name}", file=sys.stderr)
        return 1

    print("host dependency boundary check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
