#!/usr/bin/env python3
"""Find the newest older skipprd tag with the same runtime protocol.

Used by post-publish e2e to pin File / S3 / SkipprLake crate versions.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

SCRIPTS_DIR = Path(__file__).resolve().parent
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))

from plugin_workspace import resolve_plugin_workspace  # noqa: E402
from runtime_plugin_catalog import RUNTIME_PROTOCOL_VERSION_RE  # noqa: E402


KIND_LINE_RE = re.compile(r"^([ \t]*)(File|S3|SkipprLake):\s*$")
SECTION_RE = re.compile(r"^([ \t]*)(data_sources|data_sinks|schema_sinks):\s*$")
PACKAGE_VERSION_RE = re.compile(r'^version\s*=\s*"([^"]+)"\s*$')
PIN_KEYS = ("File", "S3", "SkipprLake", "SkipprLakeSchema")
COMPAT_MANIFESTS = {
    "file": Path("plugins/data_source/file/Cargo.toml"),
    "s3": Path("plugins/data_source/s3/Cargo.toml"),
    "skipprlake": Path("plugins/data_sink/skipprlake/Cargo.toml"),
    "skipprlake-schema": Path("plugins/schema_sink/skipprlake/Cargo.toml"),
}
YAML_PIN_FROM_CRATE = {
    "file": "File",
    "s3": "S3",
    "skipprlake": "SkipprLake",
    "skipprlake-schema": "SkipprLakeSchema",
}


def parse_protocol_version(source: str) -> int | None:
    match = RUNTIME_PROTOCOL_VERSION_RE.search(source)
    if match is None:
        return None
    return int(match.group(1))


def parse_package_version(toml_text: str) -> str | None:
    in_package = False
    for line in toml_text.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_package = stripped == "[package]"
            continue
        if not in_package:
            continue
        match = PACKAGE_VERSION_RE.match(stripped)
        if match is not None:
            return match.group(1)
    return None


def encode_pins(pins: dict[str, str]) -> str:
    return ",".join(f"{key}={pins[key]}" for key in PIN_KEYS if key in pins)


def decode_pins(encoded: str) -> dict[str, str]:
    pins: dict[str, str] = {}
    for part in encoded.split(","):
        part = part.strip()
        if not part or "=" not in part:
            continue
        key, value = part.split("=", 1)
        key = key.strip()
        value = value.strip().strip('"')
        if key and value:
            pins[key] = value
    return pins


def pin_runtime_plugin_versions(text: str, pins: dict[str, str]) -> str:
    lines = text.splitlines(keepends=True)
    out: list[str] = []
    section = ""
    for index, line in enumerate(lines):
        section_match = SECTION_RE.match(line.rstrip("\n"))
        if section_match is not None:
            section = section_match.group(2)
        kind_match = KIND_LINE_RE.match(line.rstrip("\n"))
        out.append(line)
        if kind_match is None:
            continue
        kind = kind_match.group(2)
        indent = kind_match.group(1)
        if kind == "SkipprLake" and section == "schema_sinks":
            version = pins.get("SkipprLakeSchema") or pins.get("SkipprLake")
        else:
            version = pins.get(kind)
        if not version:
            continue
        field_indent = indent + "  "
        nxt = lines[index + 1] if index + 1 < len(lines) else ""
        if re.match(rf"^{re.escape(field_indent)}version\s*:", nxt):
            continue
        newline = "\n" if line.endswith("\n") else ""
        out.append(f'{field_indent}version: "{version}"{newline}')
    return "".join(out)


def previous_protocol_compatible_release(
    *,
    current_tag: str,
    current_protocol: int,
    tags: list[str],
    protocol_by_tag: dict[str, int | None],
    versions_by_tag: dict[str, dict[str, str]],
    current_versions: dict[str, str],
) -> dict:
    if current_tag.endswith("-beta"):
        return {
            "skip": True,
            "prev_tag": None,
            "previous_versions": {},
            "current_versions": current_versions,
        }
    for tag in tags:
        if tag in {current_tag, "0.0.0"} or tag.startswith("test") or tag.endswith("-beta"):
            continue
        protocol = protocol_by_tag.get(tag)
        if protocol != current_protocol:
            continue
        previous_versions = versions_by_tag.get(tag, {})
        if not previous_versions:
            continue
        return {
            "skip": False,
            "prev_tag": tag,
            "previous_versions": previous_versions,
            "current_versions": current_versions,
        }
    return {"skip": True, "prev_tag": None, "previous_versions": {}, "current_versions": current_versions}


def git_output(workspace: Path, *args: str) -> str | None:
    result = subprocess.run(
        ["git", *args],
        cwd=workspace,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return None
    return result.stdout


def list_engine_tags(workspace: Path) -> list[str]:
    output = git_output(workspace, "tag", "--list", "[0-9]*", "--sort=-v:refname")
    if not output:
        return []
    return [line.strip() for line in output.splitlines() if line.strip()]


PROTOCOL_AT_TAG_CANDIDATES = (
    "crates/skippr-runtime-sdk/src/protocol.rs",
    # Pre-invert tags stored the const on the host file.
    "src/runtime_plugins/protocol.rs",
)


def protocol_at_tag(workspace: Path, tag: str) -> int | None:
    for relative in PROTOCOL_AT_TAG_CANDIDATES:
        source = git_output(workspace, "show", f"{tag}:{relative}")
        if source is None:
            continue
        version = parse_protocol_version(source)
        if version is not None:
            return version
    return None


def _manifest_versions_from_git(git_root: Path, tag: str) -> dict[str, str]:
    versions: dict[str, str] = {}
    for name, relative in COMPAT_MANIFESTS.items():
        text = git_output(git_root, "show", f"{tag}:{relative.as_posix()}")
        if text is None:
            continue
        version = parse_package_version(text)
        if version:
            versions[name] = version
    return versions


def versions_at_tag(workspace: Path, tag: str) -> dict[str, str]:
    versions = _manifest_versions_from_git(workspace, tag)
    if versions:
        return versions
    plugins = os.environ.get("SKIPPR_PLUGINS_ROOT", "").strip()
    if plugins:
        return _manifest_versions_from_git(Path(plugins), tag)
    try:
        return _manifest_versions_from_git(resolve_plugin_workspace(workspace), tag)
    except SystemExit:
        return versions


def current_workspace_versions(workspace: Path) -> dict[str, str]:
    plugins_root = resolve_plugin_workspace(workspace)
    versions: dict[str, str] = {}
    for name, relative in COMPAT_MANIFESTS.items():
        path = plugins_root / relative
        if not path.is_file():
            continue
        version = parse_package_version(path.read_text(encoding="utf-8"))
        if version:
            versions[name] = version
    return versions


def current_workspace_protocol(workspace: Path) -> int:
    from runtime_plugin_catalog import workspace_runtime_protocol_version

    return workspace_runtime_protocol_version(workspace)


def yaml_pins_from_crate_versions(versions: dict[str, str]) -> dict[str, str]:
    pins: dict[str, str] = {}
    for crate_name, yaml_key in YAML_PIN_FROM_CRATE.items():
        if crate_name in versions:
            pins[yaml_key] = versions[crate_name]
    return pins


def write_github_output(path: Path, result: dict) -> None:
    lines = [
        f"skip={'true' if result['skip'] else 'false'}",
        f"prev_tag={result.get('prev_tag') or ''}",
        f"previous_pins={encode_pins(yaml_pins_from_crate_versions(result.get('previous_versions') or {}))}",
        f"current_pins={encode_pins(yaml_pins_from_crate_versions(result.get('current_versions') or {}))}",
    ]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, default=Path.cwd())
    parser.add_argument("--current-tag", default="")
    parser.add_argument(
        "--github-output",
        action="store_true",
        help="Write skip/prev_tag/pins to $GITHUB_OUTPUT",
    )
    parser.add_argument("--pin-config", type=Path, help="YAML file to insert version: pins into")
    parser.add_argument("--pins", default="", help="File=0.1.4,S3=0.1.6,...")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    workspace = args.workspace.resolve()
    if args.pin_config is not None:
        pins = decode_pins(args.pins)
        text = args.pin_config.read_text(encoding="utf-8")
        args.pin_config.write_text(pin_runtime_plugin_versions(text, pins), encoding="utf-8")
        return 0

    current_tag = args.current_tag.strip() or os.environ.get("GITHUB_REF_NAME", "")
    current_protocol = current_workspace_protocol(workspace)
    tags = list_engine_tags(workspace)
    protocol_by_tag = {tag: protocol_at_tag(workspace, tag) for tag in tags}
    versions_by_tag = {tag: versions_at_tag(workspace, tag) for tag in tags}
    result = previous_protocol_compatible_release(
        current_tag=current_tag,
        current_protocol=current_protocol,
        tags=tags,
        protocol_by_tag=protocol_by_tag,
        versions_by_tag=versions_by_tag,
        current_versions=current_workspace_versions(workspace),
    )
    if args.github_output:
        output = os.environ.get("GITHUB_OUTPUT")
        if not output:
            raise SystemExit("GITHUB_OUTPUT is unset")
        write_github_output(Path(output), result)
    elif result["skip"]:
        print("skip=true")
    else:
        print(f"prev_tag={result['prev_tag']}")
        print(f"previous_pins={encode_pins(yaml_pins_from_crate_versions(result['previous_versions']))}")
        print(f"current_pins={encode_pins(yaml_pins_from_crate_versions(result['current_versions']))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
