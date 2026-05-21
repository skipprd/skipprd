#!/usr/bin/env python3

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from runtime_plugin_catalog import (
    load_workspace_plugin_catalog,
    manifest_payload_for_catalog_entry,
    workspace_runtime_protocol_version,
)


REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_MANIFEST_DIR = REPO_ROOT / ".skippr" / "local-runtime-plugins" / "manifests"
REACT_CARGO_REGISTRY_INDEX = "sparse+https://skippr-132355036174.d.codeartifact.us-east-1.amazonaws.com/cargo/react-cargo/"
REACT_CARGO_PATCHES = [
    ("react", "src/runtime"),
    ("react-core", "src/core"),
    ("react-http-protocol", "src/http-protocol"),
    ("react-transport", "src/transport"),
    ("react-view", "src/view"),
    ("react-module-storage-s3", "src/modules/adaptors/storage-s3"),
    ("react-module-storage-local", "src/modules/adaptors/storage-local"),
    ("react-module-storage-memory", "src/modules/adaptors/storage-memory"),
    ("react-module-provider-vector-lance", "src/modules/providers/vector-lance"),
    ("react-suite-debugger", "src/suites/suite_debugger"),
]
PLUGIN_COLLECTIONS = {
    "data_sources": "DataSource",
    "data_sinks": "DataSink",
    "deadletter_sinks": "DataSink",
    "schema_sinks": "SchemaSink",
}
PIPELINE_REF_KEYS = {
    "data_source": "data_sources",
    "input": "data_sources",
    "data_sink": "data_sinks",
    "output": "data_sinks",
    "deadletter_sink": "deadletter_sinks",
    "deadletters": "deadletter_sinks",
    "deadletter": "deadletter_sinks",
}


class LocalRuntimePluginError(RuntimeError):
    pass


@dataclass(frozen=True, order=True)
class RuntimePluginRef:
    kind: str
    plugin_name: str


@dataclass
class ConfigPluginEntry:
    plugin_name: str
    schema_sink: str | None = None


def print_step(message: str) -> None:
    print(message, file=sys.stderr)


def ensure_tool(name: str) -> str:
    path = shutil.which(name)
    if path is None:
        raise LocalRuntimePluginError(f"required tool not found on PATH: {name}")
    return path


def run_command(command: list[str], *, env: dict[str, str] | None = None) -> None:
    print_step("+ " + " ".join(command))
    subprocess.run(command, cwd=REPO_ROOT, env=env, check=True)


def local_react_root() -> Path | None:
    configured = os.environ.get("SKIPPR_REACT_ROOT", "").strip()
    candidates = []
    if configured:
        candidates.append(Path(configured))
    candidates.extend([REPO_ROOT.parent / "react", REPO_ROOT.parent.parent / "react"])
    for candidate in candidates:
        if (candidate / "Cargo.toml").exists():
            return candidate.resolve()
    return None


def react_cargo_config_args() -> list[str]:
    args = ["--config", f'registries.react-cargo.index="{REACT_CARGO_REGISTRY_INDEX}"']
    react_root = local_react_root()
    if react_root is None:
        return args
    for crate_name, relative_path in REACT_CARGO_PATCHES:
        args.extend(
            [
                "--config",
                f'patch."react-cargo".{crate_name}.path="{(react_root / relative_path).resolve()}"',
            ]
        )
    return args


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def current_rust_target_triple() -> str:
    output = subprocess.check_output([ensure_tool("rustc"), "-vV"], text=True)
    for line in output.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ").strip()
    raise LocalRuntimePluginError("failed to determine current Rust host target triple")


def target_binary_name(binary_name: str, target_triple: str) -> str:
    if "windows" in target_triple and not binary_name.endswith(".exe"):
        return f"{binary_name}.exe"
    return binary_name


def strip_inline_comment(line: str) -> str:
    in_single = False
    in_double = False
    for index, char in enumerate(line):
        if char == "'" and not in_double:
            in_single = not in_single
        elif char == '"' and not in_single:
            in_double = not in_double
        elif char == "#" and not in_single and not in_double:
            return line[:index].rstrip()
    return line.rstrip()


def yaml_key_value(line: str) -> tuple[int, str, str] | None:
    stripped = strip_inline_comment(line)
    if not stripped.strip() or ":" not in stripped:
        return None
    indent = len(stripped) - len(stripped.lstrip(" "))
    key, value = stripped.strip().split(":", 1)
    return indent, key.strip(), value.strip()


def reference_name(value: str) -> str:
    value = value.strip().strip("'\"")
    return value.rsplit(".", 1)[-1]


def parse_pipeline_refs(lines: list[str]) -> dict[str, dict[str, str]]:
    pipelines: dict[str, dict[str, str]] = {}
    active: str | None = None
    in_pipelines = False
    for line in lines:
        parsed = yaml_key_value(line)
        if parsed is None:
            continue
        indent, key, value = parsed
        if indent == 0:
            in_pipelines = key == "pipelines"
            active = None
            continue
        if not in_pipelines:
            continue
        if indent == 2:
            active = key
            pipelines.setdefault(active, {})
            continue
        if indent == 4 and active and key in PIPELINE_REF_KEYS and value:
            pipelines[active][key] = value
    return pipelines


def parse_plugin_collection(lines: list[str], section: str) -> dict[str, ConfigPluginEntry]:
    entries: dict[str, ConfigPluginEntry] = {}
    active: str | None = None
    in_section = False
    for line in lines:
        parsed = yaml_key_value(line)
        if parsed is None:
            continue
        indent, key, value = parsed
        if indent == 0:
            in_section = key == section
            active = None
            continue
        if not in_section:
            continue
        if indent == 2:
            active = key
            entries.setdefault(active, ConfigPluginEntry(plugin_name=""))
            continue
        if indent == 4 and active:
            if key == "schema_sink" and value:
                entries[active].schema_sink = reference_name(value)
            elif key not in {"schema_sink"} and not entries[active].plugin_name:
                entries[active].plugin_name = key
    return {
        name: entry
        for name, entry in entries.items()
        if entry.plugin_name
    }


def select_pipeline(pipelines: dict[str, dict[str, str]], pipeline: str | None) -> str:
    if pipeline is not None:
        if pipeline not in pipelines:
            raise LocalRuntimePluginError(
                f"pipeline {pipeline!r} was not found in config; available: {', '.join(sorted(pipelines)) or 'none'}"
            )
        return pipeline
    if len(pipelines) == 1:
        return next(iter(pipelines))
    raise LocalRuntimePluginError(
        "--pipeline is required when config contains multiple pipelines; available: "
        + (", ".join(sorted(pipelines)) or "none")
    )


def configured_runtime_plugins(config_path: Path, pipeline: str | None) -> set[RuntimePluginRef]:
    lines = config_path.read_text(encoding="utf-8").splitlines()
    pipelines = parse_pipeline_refs(lines)
    selected_pipeline = select_pipeline(pipelines, pipeline)
    collections = {
        name: parse_plugin_collection(lines, name)
        for name in PLUGIN_COLLECTIONS
    }

    selected: set[RuntimePluginRef] = set()
    for ref_key, raw_ref in pipelines[selected_pipeline].items():
        collection_name = PIPELINE_REF_KEYS[ref_key]
        entry_name = reference_name(raw_ref)
        entry = collections[collection_name].get(entry_name)
        if entry is None:
            raise LocalRuntimePluginError(
                f"pipeline {selected_pipeline!r} references {raw_ref!r}, but {entry_name!r} was not found in {collection_name}"
            )
        selected.add(RuntimePluginRef(PLUGIN_COLLECTIONS[collection_name], entry.plugin_name))
        if entry.schema_sink:
            schema_entry = collections["schema_sinks"].get(entry.schema_sink)
            if schema_entry is None:
                raise LocalRuntimePluginError(
                    f"{collection_name}.{entry_name} references schema sink {entry.schema_sink!r}, but it was not found in schema_sinks"
                )
            selected.add(RuntimePluginRef("SchemaSink", schema_entry.plugin_name))

    if not selected:
        raise LocalRuntimePluginError(
            f"pipeline {selected_pipeline!r} did not reference any runtime plugins"
        )
    return selected


def catalog_by_plugin(workspace: Path) -> dict[RuntimePluginRef, dict]:
    catalog = {}
    for entry in load_workspace_plugin_catalog(workspace):
        ref = RuntimePluginRef(entry["manifest_kind"], entry["plugin_name"])
        catalog[ref] = entry
    return catalog


def build_local_runtime_plugins(
    *,
    config_path: Path,
    pipeline: str | None,
    output_dir: Path,
    release: bool,
) -> Path:
    ensure_tool("cargo")
    selected = configured_runtime_plugins(config_path, pipeline)
    catalog = catalog_by_plugin(REPO_ROOT)
    missing = sorted(ref for ref in selected if ref not in catalog)
    if missing:
        formatted = ", ".join(f"{ref.kind}:{ref.plugin_name}" for ref in missing)
        raise LocalRuntimePluginError(
            f"configured runtime plugin(s) are not present in the workspace catalog: {formatted}"
        )

    entries = [catalog[ref] for ref in sorted(selected)]
    packages = sorted({entry["package_name"] for entry in entries})
    build_command = [ensure_tool("cargo"), "build", *react_cargo_config_args()]
    if release:
        build_command.append("--release")
    for package in packages:
        build_command.extend(["-p", package])

    build_env = os.environ.copy()
    build_env.setdefault("CARGO_INCREMENTAL", "0")
    run_command(build_command, env=build_env)

    target_dir = Path(build_env.get("CARGO_TARGET_DIR", REPO_ROOT / "target"))
    if not target_dir.is_absolute():
        target_dir = REPO_ROOT / target_dir
    profile = "release" if release else "debug"
    target_triple = current_rust_target_triple()
    protocol_version = workspace_runtime_protocol_version(REPO_ROOT)
    output_dir.mkdir(parents=True, exist_ok=True)

    for entry in entries:
        binary = target_dir / profile / target_binary_name(entry["binary_name"], target_triple)
        if not binary.exists():
            raise LocalRuntimePluginError(
                f"expected built runtime plugin binary at {binary} for {entry['plugin_name']}"
            )
        executable = str(binary.resolve())
        manifest = manifest_payload_for_catalog_entry(
            entry,
            protocol_version=protocol_version,
            artifacts={
                target_triple: {
                    "executable": executable,
                    "sha256": sha256(binary),
                }
            },
            executable=executable,
            build_checksum=entry["checksum"],
        )
        manifest_path = output_dir / entry["manifest_filename"]
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        print_step(
            f"Wrote local {entry['manifest_kind']} manifest for {entry['plugin_name']}: {manifest_path}"
        )

    print_step("")
    print_step("Use these exports before running discover/sync:")
    print_step("export USE_LOCAL_PLUGIN_CODE=1")
    print_step(f"export SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR={output_dir}")
    return output_dir


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Build runtime plugins referenced by a Skippr config and write local manifests."
    )
    parser.add_argument("--config", required=True, help="Path to a Skippr YAML config")
    parser.add_argument("--pipeline", help="Pipeline name when the config contains multiple")
    parser.add_argument(
        "--output-dir",
        default=str(DEFAULT_MANIFEST_DIR),
        help="Local manifest directory; defaults to .skippr/local-runtime-plugins/manifests",
    )
    parser.add_argument("--release", action="store_true", help="Build release plugin binaries")
    args = parser.parse_args(argv)

    try:
        manifest_dir = build_local_runtime_plugins(
            config_path=Path(args.config),
            pipeline=args.pipeline,
            output_dir=Path(args.output_dir),
            release=args.release,
        )
    except (LocalRuntimePluginError, subprocess.CalledProcessError) as err:
        print(f"local runtime plugin setup failed: {err}", file=sys.stderr)
        return 1

    print(manifest_dir)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
