#!/usr/bin/env python3
"""Python wheel semver lives in pyproject.toml, not skipprd git tags.

Beta engine tags stamp a unique PEP 440 pre-release (`x.y.zbN`) in CI so PyPI
never reuses a filename. Committed pyproject stays x.y.z.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPTS_DIR = Path(__file__).resolve().parent
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))
from engine_tag import is_beta_engine_tag, pep440_version

PYPROJECT = ROOT / "pyproject.toml"
PYTHON_CARGO = ROOT / "python" / "Cargo.toml"
PYPI_PROJECT = "skippr"
VERSION_RE = re.compile(r'(?m)^version\s*=\s*"([^"]+)"')
SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+$")
PEP440_RE = re.compile(r"^\d+\.\d+\.\d+(b[1-9]\d*)?$")


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


def stamp_python_package_version(
    version: str,
    pyproject: Path = PYPROJECT,
    cargo: Path = PYTHON_CARGO,
) -> None:
    if not PEP440_RE.fullmatch(version):
        raise SystemExit(f"python version must be x.y.z or x.y.zbN, got: {version}")
    for path in (pyproject, cargo):
        text = path.read_text(encoding="utf-8")
        updated, count = re.subn(
            r'(?m)^(version\s*=\s*")[^"]+(")',
            rf"\g<1>{version}\g<2>",
            text,
            count=1,
        )
        if count != 1:
            raise SystemExit(f"failed updating python version in {path}")
        path.write_text(updated, encoding="utf-8")


def prepare_ci_package_versions(
    ref_name: str | None = None,
    pyproject: Path = PYPROJECT,
    cargo: Path = PYTHON_CARGO,
) -> str:
    """Return the skipprd stamp version. On beta tags, also stamp unique wheel semver."""
    tag = (ref_name if ref_name is not None else os.environ.get("GITHUB_REF_NAME", "")).strip()
    if is_beta_engine_tag(tag):
        stamp_python_package_version(pep440_version(tag), pyproject=pyproject, cargo=cargo)
        return tag
    return python_semver(pyproject=pyproject, cargo=cargo)


def pypi_filenames(
    version: str,
    project: str = PYPI_PROJECT,
    urlopen=urllib.request.urlopen,
) -> set[str] | None:
    url = f"https://pypi.org/pypi/{project}/{version}/json"
    try:
        with urlopen(url, timeout=30) as response:
            payload = json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            return None
        raise
    return {item["filename"] for item in payload.get("urls", []) if "filename" in item}


def wheel_dist_dir() -> Path:
    configured = os.environ.get("SKIPPR_WHEEL_DIST")
    if configured:
        return Path(configured)
    sibling = Path("../dist")
    if sibling.is_dir():
        return sibling
    return Path("dist")


def local_wheels(dist: Path) -> list[Path]:
    return sorted(path for path in dist.glob("*.whl") if path.is_file())


WHEEL_NAME_RE = re.compile(
    r"^(?P<name>skippr)-(?P<version>\d+\.\d+\.\d+(?:b[1-9]\d*)?)(?:-(?P<build>\d[\w.]*))?-(?P<tags>.+)\.whl$"
)


def unpublished_wheels(dist: Path, published: set[str] | None) -> list[Path]:
    wheels = local_wheels(dist)
    if published is None:
        return wheels
    return [path for path in wheels if path.name not in published]


def parse_wheel_name(filename: str) -> re.Match[str] | None:
    return WHEEL_NAME_RE.fullmatch(filename)


def next_build_number(published: set[str], version: str) -> int:
    builds = [0]
    for name in published:
        match = parse_wheel_name(name)
        if match is None or match.group("version") != version:
            continue
        build = match.group("build")
        if build is None:
            continue
        try:
            builds.append(int(build.split(".", 1)[0]))
        except ValueError:
            continue
    return max(builds) + 1


def with_build_tag(filename: str, build: int) -> str:
    match = parse_wheel_name(filename)
    if match is None:
        raise ValueError(f"not a skippr wheel filename: {filename}")
    return f"skippr-{match.group('version')}-{build}-{match.group('tags')}.whl"


def wheels_to_publish(
    dist: Path,
    published: set[str] | None,
    version: str,
    *,
    allow_rebuild_stamp: bool,
) -> list[Path]:
    """Production may stamp a PEP 427 build tag. Beta versions are unique; never stamp."""
    if allow_rebuild_stamp:
        return stamp_rebuild_wheels(dist, published, version)
    return unpublished_wheels(dist, published)


def stamp_rebuild_wheels(
    dist: Path, published: set[str] | None, version: str
) -> list[Path]:
    """Keep missing-platform uploads; stamp a PEP 427 build tag when clobbering."""
    wheels = local_wheels(dist)
    if published is None:
        return wheels
    missing = unpublished_wheels(dist, published)
    if missing:
        return missing
    if not wheels:
        return []
    build = next_build_number(published, version)
    stamped: list[Path] = []
    for path in wheels:
        dest = path.with_name(with_build_tag(path.name, build))
        path.rename(dest)
        stamped.append(dest)
    return stamped


def prune_published_wheels(dist: Path, unpublished: list[Path]) -> None:
    keep = {path.resolve() for path in unpublished}
    for path in local_wheels(dist):
        if path.resolve() not in keep:
            path.unlink()


def should_publish(version: str, *, unpublished: list[str] | list[Path]) -> bool:
    return version != "0.0.0" and len(unpublished) > 0


def pypi_has_version(
    version: str,
    project: str = PYPI_PROJECT,
    urlopen=urllib.request.urlopen,
) -> bool:
    return pypi_filenames(version, project=project, urlopen=urlopen) is not None


def main() -> None:
    parser = argparse.ArgumentParser(description="Read or gate the skippr Python wheel semver.")
    parser.add_argument(
        "--skip-if-published",
        action="store_true",
        help="Write needed=true/false when any local wheel filename is new on PyPI.",
    )
    args = parser.parse_args()
    tag = os.environ.get("GITHUB_REF_NAME", "").strip()
    version = pep440_version(tag) if is_beta_engine_tag(tag) else python_semver()
    if not args.skip_if_published:
        print(version)
        return
    dist = wheel_dist_dir()
    published = pypi_filenames(version)
    missing = wheels_to_publish(
        dist,
        published,
        version,
        allow_rebuild_stamp=not is_beta_engine_tag(tag),
    )
    prune_published_wheels(dist, missing)
    needed = should_publish(version, unpublished=missing)
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(f"needed={str(needed).lower()}\n")
            handle.write(f"version={version}\n")
    print(f"python {version} publish needed={needed} unpublished={len(missing)}")


if __name__ == "__main__":
    main()
