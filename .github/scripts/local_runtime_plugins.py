#!/usr/bin/env python3

import argparse
import json
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from plugin_workspace import resolve_plugin_workspace, resolve_skipprd_workspace
from runtime_plugin_catalog import (
    load_workspace_plugin_catalog,
    manifest_payload_for_catalog_entry,
    workspace_runtime_protocol_version,
)


REPO_ROOT = resolve_skipprd_workspace(Path(__file__).resolve().parents[2])
PLUGIN_ROOT = resolve_plugin_workspace(REPO_ROOT)
DEFAULT_MANIFEST_DIR = REPO_ROOT / ".skippr" / "local-runtime-plugins" / "manifests"
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


def resolve_plugin_artifact_dir(*, skip_cargo_build: bool, env: dict[str, str] | None = None) -> Path:
    """Binaries from `cargo -p skipprd` / copied artifacts live in the host target.

    `cargo build` for plugins runs with cwd=PLUGIN_ROOT, so a relative CARGO_TARGET_DIR
    during a real build is plugin-workspace-relative. `--skip-cargo-build` looks at
    the host skipprd target (File smoke, perf cells, rust-build-release copies).
    """
    environ = env if env is not None else os.environ
    raw = str(environ.get("CARGO_TARGET_DIR", "")).strip()
    if raw:
        path = Path(raw)
        if path.is_absolute():
            return path
        base = REPO_ROOT if skip_cargo_build else PLUGIN_ROOT
        return base / path
    return (REPO_ROOT if skip_cargo_build else PLUGIN_ROOT) / "target"


def ensure_tool(name: str) -> str:
    path = shutil.which(name)
    if path is None:
        raise LocalRuntimePluginError(f"required tool not found on PATH: {name}")
    return path


def run_command(command: list[str], *, env: dict[str, str] | None = None, cwd: Path | None = None) -> None:
    print_step("+ " + " ".join(command))
    subprocess.run(command, cwd=cwd or REPO_ROOT, env=env, check=True)


def current_rust_target_triple() -> str:
    explicit = os.environ.get("SKIPPR_TARGET_TRIPLE", "").strip()
    if explicit:
        return explicit
    rustc = shutil.which("rustc")
    if rustc:
        output = subprocess.check_output([rustc, "-vV"], text=True)
        for line in output.splitlines():
            if line.startswith("host: "):
                return line.removeprefix("host: ").strip()
        raise LocalRuntimePluginError("failed to determine current Rust host target triple")
    uname = os.uname()
    sysname = uname.sysname.lower()
    machine = uname.machine
    if sysname == "linux" and machine in {"x86_64", "amd64"}:
        return "x86_64-unknown-linux-gnu"
    if sysname == "linux" and machine in {"aarch64", "arm64"}:
        return "aarch64-unknown-linux-gnu"
    if sysname == "darwin" and machine in {"arm64", "aarch64"}:
        return "aarch64-apple-darwin"
    if sysname == "darwin" and machine == "x86_64":
        return "x86_64-apple-darwin"
    raise LocalRuntimePluginError(
        "rustc is not on PATH; set SKIPPR_TARGET_TRIPLE or install rustc"
    )


def target_binary_name(binary_name: str, target_triple: str) -> str:
    if "windows" in target_triple and not binary_name.endswith(".exe"):
        return f"{binary_name}.exe"
    return binary_name


def ensure_executable(path: Path) -> None:
    mode = path.stat().st_mode
    path.chmod(mode | 0o111)


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
    skip_cargo_build: bool = False,
) -> Path:
    if not skip_cargo_build:
        ensure_tool("cargo")
    selected = configured_runtime_plugins(config_path, pipeline)
    catalog = catalog_by_plugin(PLUGIN_ROOT)
    missing = sorted(ref for ref in selected if ref not in catalog)
    if missing:
        formatted = ", ".join(f"{ref.kind}:{ref.plugin_name}" for ref in missing)
        raise LocalRuntimePluginError(
            f"configured runtime plugin(s) are not present in the workspace catalog: {formatted}"
        )

    entries = [catalog[ref] for ref in sorted(selected)]
    packages = sorted({entry["package_name"] for entry in entries})
    build_env = os.environ.copy()
    build_env.setdefault("CARGO_INCREMENTAL", "0")
    if not skip_cargo_build:
        build_command = [ensure_tool("cargo"), "build"]
        if release:
            build_command.append("--release")
        for package in packages:
            build_command.extend(["-p", package])
        run_command(build_command, env=build_env, cwd=PLUGIN_ROOT)

    target_dir = resolve_plugin_artifact_dir(skip_cargo_build=skip_cargo_build, env=build_env)
    profile = "release" if release else "debug"
    target_triple = current_rust_target_triple()
    protocol_version = workspace_runtime_protocol_version(PLUGIN_ROOT)
    output_dir.mkdir(parents=True, exist_ok=True)

    for entry in entries:
        binary = target_dir / profile / target_binary_name(entry["binary_name"], target_triple)
        if not binary.exists():
            raise LocalRuntimePluginError(
                f"expected built runtime plugin binary at {binary} for {entry['plugin_name']}"
            )
        ensure_executable(binary)
        executable = str(binary.resolve())
        manifest = manifest_payload_for_catalog_entry(
            entry,
            protocol_version=protocol_version,
            artifacts={
                target_triple: {
                    "executable": executable,
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
    parser.add_argument(
        "--skip-cargo-build",
        action="store_true",
        help="Only write manifests for binaries already built in CARGO_TARGET_DIR",
    )
    args = parser.parse_args(argv)

    try:
        manifest_dir = build_local_runtime_plugins(
            config_path=Path(args.config),
            pipeline=args.pipeline,
            output_dir=Path(args.output_dir),
            release=args.release,
            skip_cargo_build=args.skip_cargo_build,
        )
    except (LocalRuntimePluginError, subprocess.CalledProcessError) as err:
        print(f"local runtime plugin setup failed: {err}", file=sys.stderr)
        return 1

    print(manifest_dir)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
