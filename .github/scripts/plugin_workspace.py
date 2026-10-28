#!/usr/bin/env python3
"""Locate the skipprd-plugins workspace. skipprd itself has no plugins/ tree."""

from __future__ import annotations

import os
from pathlib import Path


PLUGIN_REPO_NAME = "skipprd-plugins"
ENV_PLUGINS_ROOT = "SKIPPR_PLUGINS_ROOT"
ENV_SKIPPRD_ROOT = "SKIPPR_SKIPPRD_ROOT"


def _has_plugin_tree(root: Path) -> bool:
    for kind in ("data_source", "data_sink", "schema_sink"):
        kind_dir = root / "plugins" / kind
        if kind_dir.is_dir() and any(kind_dir.glob("*/Cargo.toml")):
            return True
    return False


def _is_skipprd_host(root: Path) -> bool:
    return (root / "crates" / "skippr-runtime-sdk").is_dir()


def _is_plugin_workspace(root: Path) -> bool:
    """skipprd leftover plugins/ is illegal; the plugins repo has no host SDK crate."""
    return _has_plugin_tree(root) and not _is_skipprd_host(root)


def resolve_plugin_workspace(start: Path | None = None) -> Path:
    """Return the skipprd-plugins workspace.

    Order:
    1. SKIPPR_PLUGINS_ROOT
    2. start itself if it is the plugins repo (not skipprd)
    3. sibling directory named skipprd-plugins
    """
    env = os.environ.get(ENV_PLUGINS_ROOT, "").strip()
    if env:
        root = Path(env).expanduser().resolve()
        if not _is_plugin_workspace(root):
            raise SystemExit(
                f"{ENV_PLUGINS_ROOT}={root} is not a skipprd-plugins workspace"
            )
        return root

    here = (start or Path.cwd()).resolve()
    if _is_plugin_workspace(here):
        return here

    sibling = here.parent / PLUGIN_REPO_NAME
    if _is_plugin_workspace(sibling):
        return sibling.resolve()

    raise SystemExit(
        "runtime plugin workspace not found: clone skipprd/skipprd-plugins as a "
        f"sibling named {PLUGIN_REPO_NAME} or set {ENV_PLUGINS_ROOT}"
    )


def resolve_skipprd_workspace(start: Path | None = None) -> Path:
    """Return the skipprd host workspace (sibling of skipprd-plugins)."""
    env = os.environ.get(ENV_SKIPPRD_ROOT, "").strip()
    if env:
        root = Path(env).expanduser().resolve()
        if not _is_skipprd_host(root):
            raise SystemExit(
                f"{ENV_SKIPPRD_ROOT}={root} is missing crates/skippr-runtime-sdk"
            )
        return root

    here = (start or Path.cwd()).resolve()
    if _is_skipprd_host(here):
        return here

    sibling = here.parent / "skipprd"
    if _is_skipprd_host(sibling):
        return sibling.resolve()

    raise SystemExit(
        "skipprd host workspace not found: clone skipprd/skipprd as a sibling "
        f"or set {ENV_SKIPPRD_ROOT}"
    )
