#!/usr/bin/env python3

import re
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
PLUGIN_MANIFESTS = sorted(REPO_ROOT.glob("plugins/**/Cargo.toml"))

# These crates are connector-specific and must not leak back into the host
# dependency closure for the root `skipprd` crate, including test-only edges.
FORBIDDEN_PACKAGES = {
    "aws-sdk-athena",
    "aws-sdk-dynamodb",
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
    "jsonwebtoken",
    "lapin",
    "mongodb",
    "mysql_async",
    "native-tls",
    "pgwire-replication",
    "postgres-native-tls",
    "rdkafka",
    "rsa",
    "rumqttc",
    "ssh2",
    "tiberius",
    "tokio-postgres",
    "tokio-tungstenite",
    "tungstenite",
}

EDGE_KINDS = "normal,build,dev"


def collect_leaked_host_packages(*, all_features: bool) -> set[str]:
    command = ["cargo", "tree", "-p", "skipprd", "-e", EDGE_KINDS, "--prefix", "none"]
    if all_features:
        command.append("--all-features")
    result = subprocess.run(
        command,
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
        if package_name in FORBIDDEN_PACKAGES or package_name.startswith("skippr-plugin-"):
            leaked.add(package_name)
    return leaked


def collect_plugin_manifest_violations() -> list[str]:
    violations = []
    for manifest_path in PLUGIN_MANIFESTS:
        text = manifest_path.read_text(encoding="utf-8")
        if re.search(r"(?m)^\s*skippr\s*=", text):
            violations.append(str(manifest_path.relative_to(REPO_ROOT)))
    return violations


def main() -> int:
    leaked_default = collect_leaked_host_packages(all_features=False)
    leaked_all_features = collect_leaked_host_packages(all_features=True)
    manifest_violations = collect_plugin_manifest_violations()

    if leaked_default or leaked_all_features or manifest_violations:
        print(
            "host/plugin dependency boundary violated:",
            file=sys.stderr,
        )
        if leaked_default:
            print(
                f"  root `skipprd` leaked forbidden packages (edges: {EDGE_KINDS}):",
                file=sys.stderr,
            )
            for name in sorted(leaked_default):
                print(f"    - {name}", file=sys.stderr)
        if leaked_all_features:
            print(
                f"  root `skipprd` leaked forbidden packages under --all-features (edges: {EDGE_KINDS}):",
                file=sys.stderr,
            )
            for name in sorted(leaked_all_features):
                print(f"    - {name}", file=sys.stderr)
        if manifest_violations:
            print(
                "  plugin manifests must depend on `skippr-core`, never root `skipprd`:",
                file=sys.stderr,
            )
            for path in manifest_violations:
                print(f"    - {path}", file=sys.stderr)
        return 1

    print(
        "host/plugin dependency boundary check passed "
        f"(edges: {EDGE_KINDS}, plus plugin manifest scan and --all-features)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
