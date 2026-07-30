#!/usr/bin/env python3

import hashlib
import re
import subprocess
import sys
from pathlib import Path


def load_toml_document(path: Path) -> dict:
    try:
        import tomllib
    except ModuleNotFoundError:
        try:
            import tomli as tomllib  # type: ignore[no-redef]
        except ModuleNotFoundError:
            subprocess.check_call(
                [
                    sys.executable,
                    "-m",
                    "pip",
                    "install",
                    "--disable-pip-version-check",
                    "tomli",
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            import tomli as tomllib  # type: ignore[no-redef]
    return tomllib.loads(path.read_text(encoding="utf-8"))


PLUGIN_ROOT_PREFIXES = (
    "plugins/data_source/",
    "plugins/data_sink/",
    "plugins/schema_sink/",
)
PLAIN_SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+$")
RUNTIME_PROTOCOL_VERSION_RE = re.compile(r"pub const RUNTIME_PROTOCOL_VERSION: u32 = (\d+);")
RUNTIME_PROTOCOL_VERSION_CANDIDATES = (
    Path("src/runtime_plugins/protocol.rs"),
    Path("crates/skippr-runtime-sdk/src/protocol.rs"),
    Path("crates/skippr-core/src/runtime_plugins/protocol.rs"),
)
RUNTIME_SDK_MANIFEST = Path("crates/skippr-runtime-sdk/Cargo.toml")
RUNTIME_SDK_FINGERPRINT_SCHEME = "skippr-runtime-sdk-build-v1"
RUNTIME_SDK_GLOBAL_BUILD_INPUTS = (
    Path("Cargo.toml"),
    Path("Cargo.lock"),
    Path(".cargo/config"),
    Path(".cargo/config.toml"),
    Path("rust-toolchain"),
    Path("rust-toolchain.toml"),
)
CARGO_BUILD_DEPENDENCY_TABLES = ("dependencies", "build-dependencies")
SHARED_PLUGIN_DIR = Path("plugins/shared")
RUNTIME_PLUGIN_BUILD_INPUTS = (
    Path("Cargo.lock"),
    Path("src/runtime_plugins/protocol.rs"),
    Path("src/runtime_plugins/wire.rs"),
    Path("crates/skippr-runtime-sdk/Cargo.toml"),
    Path("crates/skippr-runtime-sdk/src"),
)
PLUGIN_KIND_SUFFIX = {
    "DataSource": "source",
    "DataSink": "sink",
    "SchemaSink": "schema",
}


def discover_plugin_manifest_paths(workspace: Path) -> list[Path]:
    manifests: list[Path] = []
    for prefix in PLUGIN_ROOT_PREFIXES:
        plugin_root = workspace / prefix
        if not plugin_root.exists():
            continue
        for manifest_path in sorted(plugin_root.glob("*/Cargo.toml")):
            main_rs = manifest_path.parent / "src" / "main.rs"
            if main_rs.exists():
                manifests.append(manifest_path.resolve())
    if not manifests:
        raise SystemExit("no runtime plugin Cargo.toml manifests found under plugins/")
    return manifests


def read_plugin_package_manifest(manifest_path: Path, workspace: Path) -> dict:
    del workspace  # kept for call-site compatibility
    document = load_toml_document(manifest_path)
    package_section = document.get("package")
    if not isinstance(package_section, dict):
        raise SystemExit(f"plugin manifest {manifest_path} is missing [package]")

    name = package_section.get("name")
    version = package_section.get("version")
    if not isinstance(name, str) or not name.strip():
        raise SystemExit(f"plugin manifest {manifest_path} is missing package.name")
    if not isinstance(version, str) or not version.strip():
        raise SystemExit(f"plugin manifest {manifest_path} is missing package.version")

    bin_sections = document.get("bin", [])
    if isinstance(bin_sections, dict):
        bin_sections = [bin_sections]
    if not isinstance(bin_sections, list):
        raise SystemExit(f"plugin manifest {manifest_path} has invalid [[bin]] tables")

    targets = []
    for bin_section in bin_sections:
        if not isinstance(bin_section, dict):
            continue
        bin_name = bin_section.get("name")
        if isinstance(bin_name, str) and bin_name.strip():
            targets.append({"name": bin_name.strip(), "kind": ["bin"]})
    if not targets:
        targets.append({"name": name, "kind": ["bin"]})

    dependencies = []
    dependency_section = document.get("dependencies", {})
    if isinstance(dependency_section, dict):
        for dependency_name, dependency_spec in dependency_section.items():
            if not isinstance(dependency_name, str) or not isinstance(dependency_spec, dict):
                continue
            dependency_path = dependency_spec.get("path")
            if not isinstance(dependency_path, str) or not dependency_path.strip():
                continue
            resolved_path = (manifest_path.parent / dependency_path).resolve()
            dependencies.append(
                {
                    "name": dependency_name,
                    "path": str(resolved_path),
                }
            )

    metadata = package_section.get("metadata")
    if not isinstance(metadata, dict):
        metadata = {}

    return {
        "name": name,
        "version": version,
        "targets": targets,
        "dependencies": dependencies,
        "metadata": metadata,
        "manifest_path": str(manifest_path.resolve()),
    }


def load_plugin_packages(workspace: Path) -> list[dict]:
    return [
        read_plugin_package_manifest(manifest_path, workspace)
        for manifest_path in discover_plugin_manifest_paths(workspace)
    ]


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


def path_checksum(path: Path) -> str:
    if path.is_dir():
        return plugin_dir_checksum(path)
    if path.is_file():
        return hashlib.sha256(path.read_bytes()).hexdigest()
    return ""


def combined_checksum(entries: list[tuple[str, str]]) -> str:
    digest = hashlib.sha256()
    for key, value in sorted(entries):
        digest.update(key.encode("utf-8"))
        digest.update(b"\0")
        digest.update(value.encode("utf-8"))
        digest.update(b"\0")
    return digest.hexdigest()


def cargo_build_dependency_specs(document: dict) -> list[tuple[str, object]]:
    dependencies: list[tuple[str, object]] = []
    for table_name in CARGO_BUILD_DEPENDENCY_TABLES:
        table = document.get(table_name, {})
        if isinstance(table, dict):
            dependencies.extend(table.items())

    target_tables = document.get("target", {})
    if isinstance(target_tables, dict):
        for target_table in target_tables.values():
            if not isinstance(target_table, dict):
                continue
            for table_name in CARGO_BUILD_DEPENDENCY_TABLES:
                table = target_table.get(table_name, {})
                if isinstance(table, dict):
                    dependencies.extend(table.items())
    return dependencies


def local_dependency_manifest_paths(
    manifest_path: Path,
    document: dict,
    *,
    workspace: Path,
    workspace_document: dict,
) -> list[Path]:
    workspace_dependencies = workspace_document.get("workspace", {}).get(
        "dependencies", {}
    )
    if not isinstance(workspace_dependencies, dict):
        workspace_dependencies = {}

    manifests = []
    for dependency_name, raw_spec in cargo_build_dependency_specs(document):
        if not isinstance(raw_spec, dict):
            continue

        dependency_spec = raw_spec
        dependency_base = manifest_path.parent
        if raw_spec.get("workspace") is True:
            workspace_spec = workspace_dependencies.get(dependency_name)
            if not isinstance(workspace_spec, dict):
                continue
            dependency_spec = workspace_spec
            dependency_base = workspace

        if dependency_spec.get("optional") is True:
            continue
        dependency_path = dependency_spec.get("path")
        if not isinstance(dependency_path, str) or not dependency_path.strip():
            continue

        dependency_dir = (dependency_base / dependency_path).resolve()
        dependency_manifest = (
            dependency_dir
            if dependency_dir.name == "Cargo.toml"
            else dependency_dir / "Cargo.toml"
        )
        if not dependency_manifest.is_file():
            raise SystemExit(
                "runtime SDK local dependency is missing Cargo.toml: "
                f"{dependency_manifest}"
            )
        try:
            dependency_manifest.relative_to(workspace)
        except ValueError as err:
            raise SystemExit(
                "runtime SDK local dependency must remain inside the workspace: "
                f"{dependency_manifest}"
            ) from err
        manifests.append(dependency_manifest)
    return sorted(set(manifests))


def cargo_package_build_input_paths(manifest_path: Path) -> list[Path]:
    package_dir = manifest_path.parent
    document = load_toml_document(manifest_path)
    inputs = [manifest_path]

    source_dir = package_dir / "src"
    if source_dir.is_dir():
        inputs.append(source_dir)

    lib_section = document.get("lib", {})
    if isinstance(lib_section, dict):
        lib_path = lib_section.get("path")
        if isinstance(lib_path, str) and lib_path.strip():
            resolved_lib_path = (package_dir / lib_path).resolve()
            if resolved_lib_path.exists() and (
                not source_dir.is_dir()
                or not resolved_lib_path.is_relative_to(source_dir)
            ):
                inputs.append(resolved_lib_path)

    package_section = document.get("package", {})
    if isinstance(package_section, dict):
        build_path = package_section.get("build", "build.rs")
        if isinstance(build_path, str) and build_path.strip():
            resolved_build_path = (package_dir / build_path).resolve()
            if resolved_build_path.is_file():
                inputs.append(resolved_build_path)

    return sorted(set(inputs))


def runtime_sdk_build_input_paths(workspace: Path) -> list[Path]:
    workspace = workspace.resolve()
    workspace_manifest = workspace / "Cargo.toml"
    sdk_manifest = workspace / RUNTIME_SDK_MANIFEST
    if not workspace_manifest.is_file():
        raise SystemExit(f"workspace is missing Cargo.toml: {workspace_manifest}")
    if not sdk_manifest.is_file():
        raise SystemExit(f"workspace is missing runtime SDK manifest: {sdk_manifest}")

    workspace_document = load_toml_document(workspace_manifest)
    inputs = [
        workspace / relative_path
        for relative_path in RUNTIME_SDK_GLOBAL_BUILD_INPUTS
        if (workspace / relative_path).exists()
    ]
    pending = [sdk_manifest.resolve()]
    visited: set[Path] = set()

    while pending:
        manifest_path = pending.pop()
        if manifest_path in visited:
            continue
        visited.add(manifest_path)
        document = load_toml_document(manifest_path)
        inputs.extend(cargo_package_build_input_paths(manifest_path))
        pending.extend(
            local_dependency_manifest_paths(
                manifest_path,
                document,
                workspace=workspace,
                workspace_document=workspace_document,
            )
        )

    return sorted(set(path.resolve() for path in inputs))


def workspace_runtime_sdk_build_fingerprint(workspace: Path) -> str:
    workspace = workspace.resolve()
    entries = [("fingerprint_scheme", RUNTIME_SDK_FINGERPRINT_SCHEME)]
    for path in runtime_sdk_build_input_paths(workspace):
        entries.append(
            (
                path.relative_to(workspace).as_posix(),
                path_checksum(path),
            )
        )
    return combined_checksum(entries)


def package_build_checksum(
    package_dir: Path, workspace: Path, extra_input_paths: list[Path] | None = None
) -> str:
    package_dir = package_dir.resolve()
    workspace = workspace.resolve()
    entries = [
        (
            package_dir.relative_to(workspace).as_posix(),
            plugin_dir_checksum(package_dir),
        )
    ]
    shared_dir = workspace / SHARED_PLUGIN_DIR
    if shared_dir.exists():
        entries.append(
            (
                SHARED_PLUGIN_DIR.as_posix(),
                plugin_dir_checksum(shared_dir),
            )
        )
    for relative_path in RUNTIME_PLUGIN_BUILD_INPUTS:
        path = workspace / relative_path
        checksum = path_checksum(path)
        if checksum:
            entries.append((relative_path.as_posix(), checksum))
    for path in sorted(extra_input_paths or []):
        resolved = path.resolve()
        if not resolved.exists():
            continue
        entries.append(
            (
                resolved.relative_to(workspace).as_posix(),
                path_checksum(resolved),
            )
        )
    return combined_checksum(entries)


def package_path_dependency_dirs(
    package: dict, packages_by_name: dict[str, dict], workspace: Path
) -> list[Path]:
    dirs = []
    package_name = package["name"]
    for dependency in package.get("dependencies", []):
        dependency_name = dependency.get("name")
        dependency_path = dependency.get("path")
        if not dependency_path:
            continue
        dependency_dir = Path(dependency_path).resolve()
        dependency_manifest_path = dependency_dir / "Cargo.toml"
        if not dependency_manifest_path.exists():
            if dependency_dir.name == "Cargo.toml":
                dependency_manifest_path = dependency_dir
                dependency_dir = dependency_dir.parent
            else:
                continue
        try:
            relative_manifest_path = dependency_manifest_path.relative_to(workspace).as_posix()
        except ValueError:
            continue
        if dependency_name == package_name:
            continue
        if not relative_manifest_path.startswith(PLUGIN_ROOT_PREFIXES):
            continue
        dirs.append(dependency_dir)
    return sorted(set(dirs))


def runtime_protocol_version_from_file(path: Path) -> int | None:
    if not path.exists():
        return None
    match = RUNTIME_PROTOCOL_VERSION_RE.search(path.read_text(encoding="utf-8"))
    if match is None:
        return None
    return int(match.group(1))


def workspace_runtime_protocol_version(workspace: Path) -> int:
    checked_paths = []
    for relative_path in RUNTIME_PROTOCOL_VERSION_CANDIDATES:
        protocol_path = workspace / relative_path
        checked_paths.append(protocol_path)
        version = runtime_protocol_version_from_file(protocol_path)
        if version is not None:
            return version

    matches = []
    for search_root in (workspace / "src", workspace / "crates"):
        if not search_root.exists():
            continue
        for protocol_path in sorted(search_root.rglob("*.rs")):
            version = runtime_protocol_version_from_file(protocol_path)
            if version is not None:
                matches.append((protocol_path, version))

    versions = {version for _, version in matches}
    if len(versions) == 1:
        return next(iter(versions))
    if len(versions) > 1:
        locations = ", ".join(
            f"{path}={version}" for path, version in matches
        )
        raise SystemExit(
            "found multiple runtime protocol versions in workspace: "
            f"{locations}"
        )

    searched = ", ".join(str(path) for path in checked_paths)
    raise SystemExit(
        "failed to determine runtime protocol version from workspace; "
        f"checked {searched}"
    )


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


def normalize_sink_capability(capability: dict) -> dict:
    normalized = dict(capability)
    grouping_support = normalized.get("grouping_support")
    if grouping_support is not None:
        normalized.setdefault(
            "supports_bounded_grouped_stream", grouping_support != "None"
        )
    return normalized


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
    if kind == "DataSink" and sink_capability is not None:
        for required_key in ("retry_semantics", "grouping_support"):
            if required_key not in sink_capability:
                raise SystemExit(
                    f"runtime sink plugin package {package['name']} sink_capability must declare {required_key}"
                )
        sink_capability = normalize_sink_capability(sink_capability)
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
        "sdk_build_fingerprint": entry["sdk_build_fingerprint"],
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


def catalog_sdk_build_fingerprint(catalog_entries: list[dict]) -> str:
    fingerprints = {
        entry.get("sdk_build_fingerprint")
        for entry in catalog_entries
        if entry.get("sdk_build_fingerprint")
    }
    if len(fingerprints) != 1 or any(
        not entry.get("sdk_build_fingerprint") for entry in catalog_entries
    ):
        raise SystemExit(
            "runtime plugin catalog must contain one shared SDK build fingerprint"
        )
    return next(iter(fingerprints))


def validate_manifest_sdk_build_fingerprint(
    manifest: dict, catalog_entry: dict, manifest_filename: str
) -> None:
    expected = catalog_entry["sdk_build_fingerprint"]
    actual = manifest.get("sdk_build_fingerprint")
    if actual != expected:
        raise SystemExit(
            f"{manifest_filename}: expected sdk_build_fingerprint {expected}, got "
            f"{actual or '<missing>'}"
        )


def validate_manifest_index_sdk_build_fingerprint(
    manifest_index: dict, catalog_entries: list[dict]
) -> None:
    expected = catalog_sdk_build_fingerprint(catalog_entries)
    actual = manifest_index.get("sdk_build_fingerprint")
    if actual != expected:
        raise SystemExit(
            "manifest index: expected sdk_build_fingerprint "
            f"{expected}, got {actual or '<missing>'}"
        )

    indexed_entries = {
        entry.get("manifest_filename"): entry
        for entry in manifest_index.get("manifests", [])
        if isinstance(entry, dict) and entry.get("manifest_filename")
    }
    for catalog_entry in catalog_entries:
        manifest_filename = catalog_entry["manifest_filename"]
        index_entry = indexed_entries.get(manifest_filename)
        if index_entry is None:
            raise SystemExit(
                f"manifest index is missing runtime plugin {manifest_filename}"
            )
        actual = index_entry.get("sdk_build_fingerprint")
        if actual != expected:
            raise SystemExit(
                f"manifest index entry {manifest_filename}: expected "
                f"sdk_build_fingerprint {expected}, got {actual or '<missing>'}"
            )


def load_workspace_plugin_catalog(workspace: Path) -> list[dict]:
    packages = load_plugin_packages(workspace)
    packages_by_name = {package["name"]: package for package in packages}
    sdk_build_fingerprint = workspace_runtime_sdk_build_fingerprint(workspace)
    catalog = []

    for package in packages:
        manifest_path = Path(package["manifest_path"]).resolve()
        rel_manifest_path = manifest_path.relative_to(workspace).as_posix()

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
                "sdk_build_fingerprint": sdk_build_fingerprint,
                "checksum": package_build_checksum(
                    manifest_path.parent,
                    workspace,
                    package_path_dependency_dirs(package, packages_by_name, workspace),
                ),
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
