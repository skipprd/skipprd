#!/usr/bin/env python3
"""Engine git tags: production x.y.z (immutable CDN) or x.y.z-beta (clobberable)."""

from __future__ import annotations

import re
from pathlib import Path

PRODUCTION_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
ENGINE_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)(-beta)?$")


def is_engine_tag(tag: str) -> bool:
    return ENGINE_RE.fullmatch(tag) is not None


def is_beta_engine_tag(tag: str) -> bool:
    return is_engine_tag(tag) and tag.endswith("-beta")


def is_production_engine_tag(tag: str) -> bool:
    return PRODUCTION_RE.fullmatch(tag) is not None and tag != "0.0.0"


def parse_production(tag: str) -> tuple[int, int, int] | None:
    match = PRODUCTION_RE.fullmatch(tag)
    if match is None or tag == "0.0.0":
        return None
    return int(match.group(1)), int(match.group(2)), int(match.group(3))


def next_beta_tag(tags: list[str]) -> str:
    versions = [parsed for parsed in (parse_production(tag) for tag in tags) if parsed is not None]
    if not versions:
        raise SystemExit("no production engine tag to base a beta on")
    major, minor, patch = max(versions)
    return f"{major}.{minor}.{patch + 1}-beta"


def release_object_prefix(tag: str) -> Path | None:
    if is_beta_engine_tag(tag):
        return Path(tag)
    return None


def should_write_latest_pointer(tag: str) -> bool:
    return is_production_engine_tag(tag)


def allow_version_path_clobber(tag: str) -> bool:
    return is_beta_engine_tag(tag)


def staged_relative_path(relative: Path, tag: str) -> Path:
    prefix = release_object_prefix(tag)
    return prefix / relative if prefix is not None else relative
