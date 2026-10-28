#!/usr/bin/env python3

import os
import re
import subprocess
import sys
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parent))
from plugin_workspace import resolve_plugin_workspace

REPO_ROOT = Path(__file__).resolve().parents[2]


def plugin_root() -> Path:
    return resolve_plugin_workspace(REPO_ROOT)


def plugin_manifests() -> list[Path]:
    return sorted(plugin_root().glob("plugins/**/Cargo.toml"))


def plugin_source_files() -> list[Path]:
    root = plugin_root()
    return sorted(
        path
        for path in root.glob("plugins/**/*")
        if path.is_file() and path.suffix in {".rs", ".toml"}
    )

# Connector plugin packages must not leak back into the host dependency
# closure for the root `skipprd` crate, including test-only edges.
# `skippr-plugin-macros` is a shared proc-macro used by the host catalog and
# runtime SDK as well as plugins; it is not a connector.
ALLOWED_HOST_PLUGIN_PREFIX_PACKAGES = {
    "skippr-plugin-macros",
}

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
    "skippr-cloud",
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
    return ["cargo"]


SDK_CRATE_DIR = REPO_ROOT / "crates" / "skippr-runtime-sdk"
SKIPPR_CORE_DIR = REPO_ROOT / "crates" / "skippr-core"
SKIPPR_CORE_IMPORT_RE = re.compile(r"\bskippr_core::")


def packages_named_in_cargo_tree(stdout: str) -> set[str]:
    names = set()
    for line in stdout.splitlines():
        match = re.match(r"^([A-Za-z0-9_.-]+) v", line.strip())
        if match:
            names.add(match.group(1))
    return names


def collect_sdk_tree_packages() -> set[str]:
    command = cargo_cmd() + [
        "tree",
        "-p",
        "skippr-runtime-sdk",
        "-e",
        EDGE_KINDS,
        "--prefix",
        "none",
    ]
    result = subprocess.run(
        command,
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return packages_named_in_cargo_tree(result.stdout)


def collect_sdk_skippr_core_imports() -> list[str]:
    hits = []
    src = SDK_CRATE_DIR / "src"
    if not src.is_dir():
        return ["crates/skippr-runtime-sdk/src missing"]
    for path in sorted(src.rglob("*.rs")):
        text = path.read_text(encoding="utf-8", errors="ignore")
        if SKIPPR_CORE_IMPORT_RE.search(text):
            hits.append(str(path.relative_to(REPO_ROOT)))
    return hits


def collect_leaked_host_packages(*, widest_features: bool) -> set[str]:
    command = cargo_cmd() + ["tree", "-p", "skipprd", "-e", EDGE_KINDS, "--prefix", "none"]
    if widest_features and HOST_WIDEST_FEATURES:
        command.extend(["--features", ",".join(HOST_WIDEST_FEATURES)])
    result = subprocess.run(
        command,
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stdout)
        sys.stderr.write(result.stderr)
        raise SystemExit(result.returncode)

    leaked = set()
    for line in result.stdout.splitlines():
        match = re.match(r"^([A-Za-z0-9_.-]+) v", line.strip())
        if not match:
            continue
        package_name = match.group(1)
        if package_name in ALLOWED_HOST_PLUGIN_PREFIX_PACKAGES:
            continue
        if package_name in FORBIDDEN_PACKAGES or package_name.startswith("skippr-plugin-"):
            leaked.add(package_name)
    return leaked


def collect_in_tree_plugin_tree() -> list[str]:
    leftover = REPO_ROOT / "plugins"
    if leftover.exists():
        return [
            "skipprd/plugins/ is illegal; runtime plugins live in skipprd/skipprd-plugins"
        ]
    return []


def collect_plugin_manifest_violations() -> list[str]:
    violations = []
    root = plugin_root()
    for manifest_path in plugin_manifests():
        text = manifest_path.read_text(encoding="utf-8")
        if re.search(r"(?m)^\s*skippr\s*=", text):
            violations.append(str(manifest_path.relative_to(root)))
        if re.search(r"(?m)^\s*skippr-core(?:\.workspace|\s*=)", text):
            violations.append(str(manifest_path.relative_to(root)))
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
    "deleted sink_compat import": re.compile(r"\bsink_compat\b"),
    "deleted source_compat import": re.compile(r"\bsource_compat\b"),
}


def collect_plugin_source_violations() -> list[str]:
    violations = []
    root = plugin_root()
    for source_path in plugin_source_files():
        relative = source_path.relative_to(root)
        text = source_path.read_text(encoding="utf-8", errors="ignore")
        for label, pattern in FORBIDDEN_PLUGIN_PATTERNS.items():
            if pattern.search(text):
                violations.append(f"{relative}: {label}")
    return violations


def main() -> int:
    leaked_default = collect_leaked_host_packages(widest_features=False)
    leaked_widest = collect_leaked_host_packages(widest_features=True)
    in_tree_plugins = collect_in_tree_plugin_tree()
    manifest_violations = collect_plugin_manifest_violations()
    source_violations = collect_plugin_source_violations()
    sdk_skippr_core_imports = collect_sdk_skippr_core_imports()
    sdk_tree = collect_sdk_tree_packages()
    sdk_depends_on_skipprd = "skipprd" in sdk_tree
    skippr_core_present = SKIPPR_CORE_DIR.exists()

    if (
        leaked_default
        or leaked_widest
        or in_tree_plugins
        or manifest_violations
        or source_violations
        or sdk_skippr_core_imports
        or sdk_depends_on_skipprd
        or skippr_core_present
    ):
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
        if in_tree_plugins:
            print("  host tree must not contain a plugins/ catalog:", file=sys.stderr)
            for path in in_tree_plugins:
                print(f"    - {path}", file=sys.stderr)
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
        if skippr_core_present:
            print(
                "  skippr-core crate is illegal (pub use skipprd); delete crates/skippr-core",
                file=sys.stderr,
            )
        if sdk_skippr_core_imports:
            print(
                "  skippr-runtime-sdk must not import skippr_core:::",
                file=sys.stderr,
            )
            for path in sdk_skippr_core_imports:
                print(f"    - {path}", file=sys.stderr)
        if sdk_depends_on_skipprd:
            print(
                "  cargo tree -p skippr-runtime-sdk must not contain skipprd",
                file=sys.stderr,
            )
        return 1

    print(
        "host/plugin dependency boundary check passed "
        f"(edges: {EDGE_KINDS}, plus plugin manifest scan and widest host features)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
