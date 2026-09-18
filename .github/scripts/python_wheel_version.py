#!/usr/bin/env python3
"""Python wheel semver lives in pyproject.toml, not skipprd git tags."""

from __future__ import annotations

import argparse
import os
import re
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PYPROJECT = ROOT / "pyproject.toml"
PYTHON_CARGO = ROOT / "python" / "Cargo.toml"
PYPI_PROJECT = "skippr"
VERSION_RE = re.compile(r'(?m)^version\s*=\s*"([^"]+)"')
SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+$")


def read_first_version(path: Path) -> str:
    match = VERSION_RE.search(path.read_text(encoding="utf-8"))
    if match is None:
        raise SystemExit(f"no version field in {path}")
    version = match.group(1)
    if not SEMVER_RE.fullmatch(version):
        raise SystemExit(f"version in {path} must be x.y.z, got: {version}")
    return version


def python_semver(pyproject: Path = PYPROJECT, cargo: Path = PYTHON_CARGO) -> str:
    pyproject_version = read_first_version(pyproject)
    cargo_version = read_first_version(cargo)
    if pyproject_version != cargo_version:
        raise SystemExit(
            f"python semver mismatch: {pyproject}={pyproject_version} {cargo}={cargo_version}"
        )
    return pyproject_version


def pypi_has_version(
    version: str,
    project: str = PYPI_PROJECT,
    urlopen=urllib.request.urlopen,
) -> bool:
    url = f"https://pypi.org/pypi/{project}/{version}/json"
    try:
        with urlopen(url, timeout=30) as response:
            return getattr(response, "status", 200) == 200
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            return False
        raise


def should_publish(version: str, *, published: bool) -> bool:
    return version != "0.0.0" and not published


def main() -> None:
    parser = argparse.ArgumentParser(description="Read or gate the skippr Python wheel semver.")
    parser.add_argument(
        "--skip-if-published",
        action="store_true",
        help="Write needed=true/false for GitHub Actions when this version is new on PyPI.",
    )
    args = parser.parse_args()
    version = python_semver()
    if not args.skip_if_published:
        print(version)
        return
    needed = should_publish(version, published=pypi_has_version(version))
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(f"needed={str(needed).lower()}\n")
            handle.write(f"version={version}\n")
    print(f"python {version} publish needed={needed}")


if __name__ == "__main__":
    main()
