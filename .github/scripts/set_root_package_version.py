#!/usr/bin/env python3
from __future__ import annotations

import argparse
import re
from pathlib import Path


SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+$")


def normalize_semver(version: str) -> str:
    stripped = version[1:] if version.startswith("v") else version
    if not SEMVER_RE.fullmatch(stripped):
        raise SystemExit(f"version must be plain semver (x.y.z), got: {version}")
    return stripped


def replace_manifest_package_version(cargo_toml_path: Path, package_name: str, version: str) -> None:
    text = cargo_toml_path.read_text(encoding="utf-8")
    updated, count = re.subn(
        rf'(?ms)^(\[package\]\s+name\s*=\s*"{re.escape(package_name)}"\s+version\s*=\s*")[^"]+(")',
        rf"\g<1>{version}\g<2>",
        text,
        count=1,
    )
    if count != 1:
        raise SystemExit(f"failed updating {package_name} version in {cargo_toml_path}")
    cargo_toml_path.write_text(updated, encoding="utf-8")


def replace_lockfile_package_version(cargo_lock_path: Path, package_name: str, version: str) -> None:
    text = cargo_lock_path.read_text(encoding="utf-8")
    pattern = re.compile(
        rf'(?ms)^(\[\[package\]\]\s+name\s*=\s*"{re.escape(package_name)}"\s+version\s*=\s*")[^"]+(")'
    )
    updated, count = pattern.subn(rf"\g<1>{version}\g<2>", text, count=1)
    if count != 1:
        raise SystemExit(f"failed updating {package_name} version in {cargo_lock_path}")
    cargo_lock_path.write_text(updated, encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser(description="Set release package versions in checked-in manifests.")
    parser.add_argument("--workspace", required=True, help="Workspace root containing Cargo.toml and Cargo.lock")
    parser.add_argument("--version", required=True, help="Semver version to write (optional v prefix)")
    args = parser.parse_args()

    version = normalize_semver(args.version)
    workspace = Path(args.workspace).resolve()
    replace_manifest_package_version(workspace / "Cargo.toml", "skipprd", version)
    replace_lockfile_package_version(workspace / "Cargo.lock", "skipprd", version)


if __name__ == "__main__":
    main()
