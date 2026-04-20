#!/usr/bin/env python3

import hashlib
import json
import re
import subprocess
from pathlib import Path


PLUGIN_ROOT_PREFIXES = (
    "plugins/data_source/",
    "plugins/data_sink/",
    "plugins/schema_sink/",
)
PLAIN_SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+$")
RUNTIME_PROTOCOL_VERSION_RE = re.compile(r"pub const RUNTIME_PROTOCOL_VERSION: u32 = (\d+);")
WORKSPACE_BUILD_INPUT_FILES = (
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
)
PLUGIN_KIND_SUFFIX = {
    "DataSource": "source",
    "DataSink": "sink",
    "SchemaSink": "schema",
}


def cargo_metadata(workspace: Path) -> dict:
    return json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            cwd=workspace,
            text=True,
        )
    )


def plugin_dir_checksum(plugin_dir: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(plugin_dir.rglob("*")):
        if not path.is_file():
            continue
        rel_path = path.relative_to(plugin_dir).as_posix()
        if rel_path.startswith("target/"):
            continue
        digest.update(rel_path.encode("utf-8"))
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()


def workspace_input_digests(workspace: Path) -> tuple[tuple[str, str], ...]:
    digests = []
    for relative_path in WORKSPACE_BUILD_INPUT_FILES:
        path = workspace / relative_path
        if not path.exists():
            continue
        digests.append((relative_path, hashlib.sha256(path.read_bytes()).hexdigest()))
    return tuple(digests)


def local_workspace_packages(metadata: dict, workspace: Path) -> dict[str, dict]:
    packages = {}
    for package in metadata["packages"]:
        manifest_path = Path(package["manifest_path"]).resolve()
        try:
            package_dir = manifest_path.parent
            relative_dir = package_dir.relative_to(workspace).as_posix()
        except ValueError:
            continue

        packages[package["id"]] = {
            "name": package["name"],
            "relative_dir": relative_dir,
            "dir_checksum": plugin_dir_checksum(package_dir),
        }
    return packages


def dependency_graph(metadata: dict) -> dict[str, tuple[str, ...]]:
    package_ids_by_dir = {}
    for package in metadata["packages"]:
        manifest_path = Path(package["manifest_path"]).resolve()
        package_ids_by_dir[manifest_path.parent] = package["id"]

    graph = {}
    for package in metadata["packages"]:
        dependency_ids = set()
        for dependency in package.get("dependencies", []):
            dependency_path = dependency.get("path")
            if not dependency_path:
                continue
            dependency_id = package_ids_by_dir.get(Path(dependency_path).resolve())
            if dependency_id:
                dependency_ids.add(dependency_id)
        graph[package["id"]] = tuple(sorted(dependency_ids))
    return graph


def local_dependency_closure(
    package_id: str,
    dependency_graph_by_id: dict[str, tuple[str, ...]],
    local_packages: dict[str, dict],
) -> tuple[str, ...]:
    seen = set()
    stack = [package_id]

    while stack:
        current = stack.pop()
        if current in seen or current not in local_packages:
            continue
        seen.add(current)
        for dependency_id in dependency_graph_by_id.get(current, ()): 
            if dependency_id in local_packages and dependency_id not in seen:
                stack.append(dependency_id)

    return tuple(sorted(seen, key=lambda current: local_packages[current]["relative_dir"]))


def combined_checksum(entries: list[tuple[str, str]]) -> str:
    digest = hashlib.sha256()
    for key, value in sorted(entries):
        digest.update(key.encode("utf-8"))
        digest.update(b"\0")
        digest.update(value.encode("utf-8"))
        digest.update(b"\0")
    return digest.hexdigest()


def package_dependency_checksum(
    package_id: str,
    dependency_graph_by_id: dict[str, tuple[str, ...]],
    local_packages: dict[str, dict],
    workspace_inputs: tuple[tuple[str, str], ...],
) -> str:
    entries = [
        (
            local_packages[dependency_id]["relative_dir"],
            local_packages[dependency_id]["dir_checksum"],
        )
        for dependency_id in local_dependency_closure(
            package_id, dependency_graph_by_id, local_packages
        )
    ]
    entries.extend(workspace_inputs)
    return combined_checksum(entries)


def workspace_runtime_protocol_version(workspace: Path) -> int:
    protocol_path = workspace / "crates" / "skippr-runtime-sdk" / "src" / "protocol.rs"
    match = RUNTIME_PROTOCOL_VERSION_RE.search(protocol_path.read_text(encoding="utf-8"))
    if match is None:
        raise SystemExit(
            f"failed to determine runtime protocol version from {protocol_path}"
        )
    return int(match.group(1))


def runtime_plugin_slug(relative_dir: str) -> str:
    return Path(relative_dir).name.replace("_", "-")


def manifest_filename_for_plugin(relative_dir: str, kind: str) -> str:
    suffix = PLUGIN_KIND_SUFFIX.get(kind)
    if suffix is None:
        raise SystemExit(f"unsupported runtime plugin kind {kind!r}")
    return f"{runtime_plugin_slug(relative_dir)}-{suffix}.json"


def manifest_name_for_plugin(relative_dir: str, kind: str) -> str:
    suffix = PLUGIN_KIND_SUFFIX.get(kind)
    if suffix is None:
        raise SystemExit(f"unsupported runtime plugin kind {kind!r}")
    return f"{runtime_plugin_slug(relative_dir)}-runtime-{suffix}"


def versioned_manifest_relative_path(entry: dict) -> Path:
    return (
        Path("plugins")
        / entry["manifest_stem"]
        / "versions"
        / entry["package_version"]
        / entry["manifest_filename"]
    )


def versioned_artifact_relative_path(entry: dict, target_triple: str, binary_name: str) -> Path:
    return (
        Path("plugins")
        / entry["manifest_stem"]
        / "versions"
        / entry["package_version"]
        / target_triple
        / binary_name
    )


def plugin_metadata_for_package(package: dict) -> dict:
    plugin_metadata = package.get("metadata", {}).get("skippr-plugin")
    if not isinstance(plugin_metadata, dict):
        raise SystemExit(
            f"runtime plugin package {package['name']} must define [package.metadata.skippr-plugin]"
        )
    return plugin_metadata


def normalized_plugin_args(value: object) -> list[str]:
    if value is None:
        return []
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise SystemExit("runtime plugin metadata args must be a string list")
    return list(value)


def capability_descriptor(value: object, key: str) -> dict | None:
    if value is None:
        return None
    if not isinstance(value, dict):
        raise SystemExit(f"runtime plugin metadata {key} must be a table/object")
    return dict(value)


def validate_plugin_metadata(package: dict, plugin_metadata: dict) -> dict:
    kind = plugin_metadata.get("kind")
    if kind not in PLUGIN_KIND_SUFFIX:
        raise SystemExit(
            f"runtime plugin package {package['name']} must declare a valid kind; got {kind!r}"
        )

    plugin_name = plugin_metadata.get("plugin_name")
    if not isinstance(plugin_name, str) or not plugin_name.strip():
        raise SystemExit(
            f"runtime plugin package {package['name']} must declare plugin_name"
        )

    source_capability = capability_descriptor(
        plugin_metadata.get("source_capability"), "source_capability"
    )
    sink_capability = capability_descriptor(
        plugin_metadata.get("sink_capability"), "sink_capability"
    )
    if kind == "DataSource" and source_capability is None:
        raise SystemExit(
            f"runtime source plugin package {package['name']} must declare source_capability"
        )
    if kind == "DataSink" and sink_capability is None:
        raise SystemExit(
            f"runtime sink plugin package {package['name']} must declare sink_capability"
        )
    if kind == "SchemaSink" and (source_capability or sink_capability):
        raise SystemExit(
            f"runtime schema plugin package {package['name']} cannot declare source/sink capabilities"
        )

    supports_schema = plugin_metadata.get("supports_schema", False)
    if not isinstance(supports_schema, bool):
        raise SystemExit(
            f"runtime plugin package {package['name']} supports_schema must be boolean"
        )

    config_schema_version = plugin_metadata.get("config_schema_version", 1)
    if not isinstance(config_schema_version, int):
        raise SystemExit(
            f"runtime plugin package {package['name']} config_schema_version must be an integer"
        )

    return {
        "kind": kind,
        "plugin_name": plugin_name.strip(),
        "supports_schema": supports_schema,
        "config_schema_version": config_schema_version,
        "args": normalized_plugin_args(plugin_metadata.get("args")),
        "source_capability": source_capability,
        "sink_capability": sink_capability,
    }


def manifest_payload_for_catalog_entry(
    entry: dict,
    *,
    protocol_version: int,
    artifacts: dict,
    executable: str | None = None,
    build_checksum: str | None = None,
) -> dict:
    manifest = {
        "name": entry["manifest_name"],
        "kind": entry["manifest_kind"],
        "plugin_name": entry["plugin_name"],
        "version": entry["package_version"],
        "protocol_version": protocol_version,
        "config_schema_version": entry["config_schema_version"],
        "artifacts": artifacts,
        "args": list(entry["args"]),
        "supports_schema": entry["supports_schema"],
    }
    if executable is not None:
        manifest["executable"] = executable
    if build_checksum is not None:
        manifest["build_checksum"] = build_checksum
    if entry["source_capability"] is not None:
        manifest["source_capability"] = dict(entry["source_capability"])
    if entry["sink_capability"] is not None:
        manifest["sink_capability"] = dict(entry["sink_capability"])
    return manifest


def load_workspace_plugin_catalog(workspace: Path) -> list[dict]:
    metadata = cargo_metadata(workspace)
    local_packages = local_workspace_packages(metadata, workspace)
    dependency_graph_by_id = dependency_graph(metadata)
    workspace_inputs = workspace_input_digests(workspace)
    checksum_cache: dict[str, str] = {}
    catalog = []

    def checksum_for_package(package_id: str) -> str:
        cached = checksum_cache.get(package_id)
        if cached is not None:
            return cached
        checksum = package_dependency_checksum(
            package_id,
            dependency_graph_by_id,
            local_packages,
            workspace_inputs,
        )
        checksum_cache[package_id] = checksum
        return checksum

    for package in metadata["packages"]:
        manifest_path = Path(package["manifest_path"]).resolve()
        rel_manifest_path = manifest_path.relative_to(workspace).as_posix()
        if not rel_manifest_path.startswith(PLUGIN_ROOT_PREFIXES):
            continue

        bin_targets = [target["name"] for target in package["targets"] if "bin" in target["kind"]]
        if not bin_targets:
            continue
        if len(bin_targets) != 1 or bin_targets[0] != package["name"]:
            raise SystemExit(
                "runtime plugin catalog expects each plugin crate to expose exactly one "
                f"bin target matching the package name; got {package['name']} with {bin_targets}"
            )
        if not PLAIN_SEMVER_RE.fullmatch(package["version"]):
            raise SystemExit(
                f"runtime plugin package {package['name']} must use a checked-in plain semver "
                f"version in Cargo.toml; got {package['version']}"
            )

        validated = validate_plugin_metadata(package, plugin_metadata_for_package(package))
        relative_dir = manifest_path.parent.relative_to(workspace).as_posix()
        manifest_filename = manifest_filename_for_plugin(relative_dir, validated["kind"])
        catalog.append(
            {
                "package_name": package["name"],
                "package_dir": relative_dir,
                "package_version": package["version"],
                "checksum": checksum_for_package(package["id"]),
                "binary_name": package["name"],
                "manifest_filename": manifest_filename,
                "manifest_stem": manifest_filename.removesuffix('.json'),
                "manifest_name": manifest_name_for_plugin(relative_dir, validated["kind"]),
                "manifest_kind": validated["kind"],
                "plugin_name": validated["plugin_name"],
                "supports_schema": validated["supports_schema"],
                "config_schema_version": validated["config_schema_version"],
                "args": validated["args"],
                "source_capability": validated["source_capability"],
                "sink_capability": validated["sink_capability"],
            }
        )

    if not catalog:
        raise SystemExit("no runtime plugin packages found")
    return sorted(catalog, key=lambda entry: entry["manifest_filename"])
