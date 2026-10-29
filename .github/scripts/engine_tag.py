#!/usr/bin/env python3
"""Engine git tags: production x.y.z or immutable x.y.z-beta-N (monotonic N)."""

from __future__ import annotations

import re
from pathlib import Path

PRODUCTION_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
BETA_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)-beta-([1-9]\d*)$")
ENGINE_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)(-beta-[1-9]\d*)?$")
# Shared with install.sh / rust-build-release bash `=~` checks.
ENGINE_TAG_BASH_RE = r"^[0-9]+\.[0-9]+\.[0-9]+(-beta-[1-9][0-9]*)?$"


def is_engine_tag(tag: str) -> bool:
    return ENGINE_RE.fullmatch(tag) is not None


def is_beta_engine_tag(tag: str) -> bool:
    return BETA_RE.fullmatch(tag) is not None


def is_production_engine_tag(tag: str) -> bool:
    return PRODUCTION_RE.fullmatch(tag) is not None and tag != "0.0.0"


def parse_production(tag: str) -> tuple[int, int, int] | None:
    match = PRODUCTION_RE.fullmatch(tag)
    if match is None or tag == "0.0.0":
        return None
    return int(match.group(1)), int(match.group(2)), int(match.group(3))


def parse_beta(tag: str) -> tuple[int, int, int, int] | None:
    match = BETA_RE.fullmatch(tag)
    if match is None:
        return None
    return (
        int(match.group(1)),
        int(match.group(2)),
        int(match.group(3)),
        int(match.group(4)),
    )


def pep440_version(tag: str) -> str:
    """PyPI/PEP 440 spelling of an engine tag. Beta N → xbN (not a clobberable filename)."""
    parsed = parse_beta(tag)
    if parsed is not None:
        major, minor, patch, n = parsed
        return f"{major}.{minor}.{patch}b{n}"
    if not is_engine_tag(tag):
        raise SystemExit(f"not an engine tag: {tag}")
    return tag


def next_beta_tag(tags: list[str]) -> str:
    versions = [parsed for parsed in (parse_production(tag) for tag in tags) if parsed is not None]
    if not versions:
        raise SystemExit("no production engine tag to base a beta on")
    major, minor, patch = max(versions)
    next_patch = patch + 1
    ns = [
        n
        for parsed in (parse_beta(tag) for tag in tags)
        if parsed is not None
        for m, i, p, n in (parsed,)
        if (m, i, p) == (major, minor, next_patch)
    ]
    n = max(ns, default=0) + 1
    return f"{major}.{minor}.{next_patch}-beta-{n}"


def release_object_prefix(tag: str) -> Path | None:
    if is_beta_engine_tag(tag):
        return Path(tag)
    return None


def should_write_latest_pointer(tag: str) -> bool:
    return is_production_engine_tag(tag)


def allow_version_path_clobber(_tag: str) -> bool:
    return False


def staged_relative_path(relative: Path, tag: str) -> Path:
    prefix = release_object_prefix(tag)
    return prefix / relative if prefix is not None else relative
