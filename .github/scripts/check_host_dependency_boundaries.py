#!/usr/bin/env python3

import os
import re
import subprocess
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
PLUGIN_MANIFESTS = sorted(REPO_ROOT.glob("plugins/**/Cargo.toml"))
PLUGIN_SOURCE_FILES = sorted(
    path
    for path in REPO_ROOT.glob("plugins/**/*")
    if path.is_file() and path.suffix in {".rs", ".toml"}
)

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

# Widest routine host build for CI (not `cargo tree --all-features`). Release-only
# features such as `offset-store-dynamodb` intentionally pull connector SDKs and
# are enabled only in published skipprd binaries.
HOST_WIDEST_FEATURES = ["stats_integration"]


def cargo_cmd() -> list[str]:
    override = os.environ.get("CARGO")
    if override:
        return override.split()
    wrapper = REPO_ROOT / "scripts" / "cargo-with-local-react.sh"
    if wrapper.is_file():
        return [str(wrapper)]
    return ["cargo"]


def collect_leaked_host_packages(*, widest_features: bool) -> set[str]:
    command = cargo_cmd() + ["tree", "-p", "skipprd", "-e", EDGE_KINDS, "--prefix", "none"]
    if widest_features and HOST_WIDEST_FEATURES:
        command.extend(["--features", ",".join(HOST_WIDEST_FEATURES)])
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
        if re.search(r"(?m)^\s*skippr-core(?:\.workspace|\s*=)", text):
            violations.append(str(manifest_path.relative_to(REPO_ROOT)))
    return violations


FORBIDDEN_PLUGIN_PATTERNS = {
    "direct skippr_core import": re.compile(r"\bskippr_core::"),
    "core ingest import": re.compile(r"\b(?:crate::|skippr_runtime_sdk::)ingest_work\b"),
    "core metadata singleton import": re.compile(
        r"\b(?:crate::|skippr_runtime_sdk::)(?:METADATA|PIPELINE_SCHEMA_VERSION)\b"
    ),
    "core buffer module import": re.compile(r"\b(?:crate::|skippr_runtime_sdk::)buffer\b"),
    "core compactor module import": re.compile(r"\b(?:crate::|skippr_runtime_sdk::)engine\b"),
    "core runtime host import": re.compile(r"\b(?:crate::|skippr_runtime_sdk::)runtime_plugins::host\b"),
    "offset writer API import": re.compile(
        r"\b(?:crate::|skippr_runtime_sdk::)helpers::offsets::.*\bOffsets\b"
    ),
    "source checkpoint write": re.compile(r"\.store_checkpoint_(?:payload|envelope)\s*\("),
    "source offset write": re.compile(r"\boffsets\s*\.\s*set\s*\("),
    "plugin ingest_file": re.compile(r"\.ingest_file\s*\("),
    "runtime ingest relay": re.compile(r"\bRuntimeIngestRelay\b"),
    "relay raw ingest": re.compile(r"\brelay_raw_ingest_tasks\b"),
}


def collect_plugin_source_violations() -> list[str]:
    violations = []
    for source_path in PLUGIN_SOURCE_FILES:
        relative = source_path.relative_to(REPO_ROOT)
        text = source_path.read_text(encoding="utf-8", errors="ignore")
        for label, pattern in FORBIDDEN_PLUGIN_PATTERNS.items():
            if pattern.search(text):
                violations.append(f"{relative}: {label}")
    return violations


def main() -> int:
    leaked_default = collect_leaked_host_packages(widest_features=False)
    leaked_widest = collect_leaked_host_packages(widest_features=True)
    manifest_violations = collect_plugin_manifest_violations()
    source_violations = collect_plugin_source_violations()

    if leaked_default or leaked_widest or manifest_violations or source_violations:
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
        if leaked_widest:
            print(
                f"  root `skipprd` leaked forbidden packages under widest host features {HOST_WIDEST_FEATURES} (edges: {EDGE_KINDS}):",
                file=sys.stderr,
            )
            for name in sorted(leaked_widest):
                print(f"    - {name}", file=sys.stderr)
        if manifest_violations:
            print(
                "  plugin manifests must depend only on the narrow runtime SDK, never root `skipprd` or `skippr-core`:",
                file=sys.stderr,
            )
            for path in manifest_violations:
                print(f"    - {path}", file=sys.stderr)
        if source_violations:
            print(
                "  plugin sources must not import host/core internals:",
                file=sys.stderr,
            )
            for violation in source_violations:
                print(f"    - {violation}", file=sys.stderr)
        return 1

    print(
        "host/plugin dependency boundary check passed "
        f"(edges: {EDGE_KINDS}, plus plugin manifest scan and widest host features)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
