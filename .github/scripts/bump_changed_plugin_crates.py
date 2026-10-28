#!/usr/bin/env python3
"""Bump plugin crate patch versions when checksum inputs changed since a git base.

CDN version paths are immutable. A changed plugin (or shared protocol/SDK/shared
dir) must ship a new crate patch before the engine tag, or the planner
SystemExits. Everyday commits do not run this; /skippr-release does.
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

from runtime_plugin_catalog import (
    PLUGIN_ROOT_PREFIXES,
    RUNTIME_PLUGIN_BUILD_INPUTS,
    SHARED_PLUGIN_DIR,
    discover_plugin_manifest_paths,
)


PLAIN_PATCH_SEMVER_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
PACKAGE_VERSION_LINE_RE = re.compile(r'^version\s*=\s*"([^"]+)"\s*$')


def bump_patch(version: str) -> str:
    match = PLAIN_PATCH_SEMVER_RE.fullmatch(version.strip())
    if match is None:
        raise SystemExit(f"plugin version {version!r} is not x.y.z; bump it by hand")
    major, minor, patch = match.groups()
    return f"{major}.{minor}.{int(patch) + 1}"


def parse_xyz(version: str) -> tuple[int, int, int]:
    match = PLAIN_PATCH_SEMVER_RE.fullmatch(version.strip())
    if match is None:
        raise SystemExit(f"plugin version {version!r} is not x.y.z; bump it by hand")
    return int(match.group(1)), int(match.group(2)), int(match.group(3))


def next_publishable_version(current: str, published: str) -> str:
    """New patch that is strictly newer than both the working tree and last published."""
    nxt = bump_patch(current)
    if parse_xyz(nxt) <= parse_xyz(published):
        return bump_patch(published)
    return nxt


def posix_path(path: str) -> str:
    path = path.replace("\\", "/")
    while path.startswith("./"):
        path = path[2:]
    return path


def is_shared_build_input(relative: str) -> bool:
    relative = posix_path(relative)
    shared = SHARED_PLUGIN_DIR.as_posix().rstrip("/")
    if relative == shared or relative.startswith(f"{shared}/"):
        return True
    for input_path in RUNTIME_PLUGIN_BUILD_INPUTS:
        posix = input_path.as_posix()
        if relative == posix:
            return True
        if posix.endswith("/src") or not posix.endswith(".rs"):
            if relative.startswith(f"{posix.rstrip('/')}/"):
                return True
    return False


def plugin_rel_dir_for_path(relative: str) -> str | None:
    relative = posix_path(relative)
    for prefix in PLUGIN_ROOT_PREFIXES:
        if not relative.startswith(prefix):
            continue
        rest = relative[len(prefix) :]
        crate = rest.split("/", 1)[0]
        if crate:
            return f"{prefix}{crate}".rstrip("/")
    return None


def catalog_rel_dirs(workspace: Path) -> dict[str, Path]:
    from runtime_plugin_catalog import catalog_plugin_workspace

    plugin_ws = catalog_plugin_workspace(workspace).resolve()
    dirs: dict[str, Path] = {}
    for manifest in discover_plugin_manifest_paths(workspace):
        rel = manifest.parent.relative_to(plugin_ws).as_posix()
        dirs[rel] = manifest
    return dirs


def read_package_version(manifest_path: Path) -> str:
    in_package = False
    for line in manifest_path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_package = stripped == "[package]"
            continue
        if not in_package:
            continue
        match = PACKAGE_VERSION_LINE_RE.match(stripped)
        if match is not None:
            return match.group(1)
    raise SystemExit(f"{manifest_path} has no [package] version")


def replace_package_version(text: str, new_version: str) -> str:
    in_package = False
    replaced = False
    lines: list[str] = []
    for line in text.splitlines(keepends=True):
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_package = stripped == "[package]"
        elif in_package and not replaced and PACKAGE_VERSION_LINE_RE.match(stripped):
            indent = line[: len(line) - len(line.lstrip())]
            newline = "\n" if line.endswith("\n") else ""
            line = f'{indent}version = "{new_version}"{newline}'
            replaced = True
        lines.append(line)
    if not replaced:
        raise SystemExit("Cargo.toml is missing [package] version")
    return "".join(lines)


def git_env(workspace: Path | None = None) -> dict[str, str]:
    """Drop inherited GIT_* so nested git uses cwd, not a parent hook's repo."""
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("GIT_"):
            del env[key]
    if workspace is not None:
        env["GIT_DIR"] = str(workspace / ".git")
        env["GIT_WORK_TREE"] = str(workspace.resolve())
    return env


def _bump_git_roots(workspace: Path) -> list[Path]:
    from plugin_workspace import resolve_plugin_workspace, resolve_skipprd_workspace

    roots: list[Path] = []
    seen: set[Path] = set()

    def add(root: Path) -> None:
        resolved = root.resolve()
        if resolved in seen:
            return
        seen.add(resolved)
        roots.append(resolved)

    add(workspace)
    try:
        add(resolve_plugin_workspace(workspace))
    except SystemExit:
        pass
    try:
        add(resolve_skipprd_workspace(workspace))
    except SystemExit:
        pass
    return roots


def _changed_in_repo(git_root: Path, base: str) -> list[str] | None:
    env = git_env(git_root)
    probe = subprocess.run(
        ["git", "rev-parse", "--verify", f"{base}^{{commit}}"],
        cwd=git_root,
        capture_output=True,
        env=env,
    )
    if probe.returncode != 0:
        return None
    seen: set[str] = set()
    commands = (
        ["git", "diff", "--name-only", base],
        ["git", "diff", "--name-only", "--cached", base],
        ["git", "ls-files", "--others", "--exclude-standard"],
    )
    for command in commands:
        output = subprocess.check_output(command, cwd=git_root, text=True, env=env)
        for line in output.splitlines():
            path = posix_path(line.strip())
            if path:
                seen.add(path)
    return sorted(seen)


def git_changed_paths(workspace: Path, base: str) -> list[str]:
    seen: set[str] = set()
    found = False
    for root in _bump_git_roots(workspace):
        paths = _changed_in_repo(root, base)
        if paths is None:
            continue
        found = True
        seen.update(paths)
    if not found:
        raise SystemExit(
            f"git ref {base} not found in skipprd or skipprd-plugins"
        )
    return sorted(seen)


def _package_version_from_git(git_root: Path, base: str, rel_dir: str) -> str | None:
    git_path = f"{rel_dir}/Cargo.toml"
    result = subprocess.run(
        ["git", "show", f"{base}:{git_path}"],
        cwd=git_root,
        text=True,
        capture_output=True,
        env=git_env(git_root),
    )
    if result.returncode != 0:
        return None
    in_package = False
    for line in result.stdout.splitlines():
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_package = stripped == "[package]"
            continue
        if not in_package:
            continue
        match = PACKAGE_VERSION_LINE_RE.match(stripped)
        if match is not None:
            return match.group(1)
    return None


def git_base_package_version(workspace: Path, base: str, rel_dir: str) -> str | None:
    for root in _bump_git_roots(workspace):
        version = _package_version_from_git(root, base, rel_dir)
        if version is not None:
            return version
    return None


def packages_selected_for_bump(
    catalog: dict[str, Path], changed_paths: list[str]
) -> list[str]:
    if any(is_shared_build_input(path) for path in changed_paths):
        return sorted(catalog)
    selected: set[str] = set()
    for path in changed_paths:
        rel_dir = plugin_rel_dir_for_path(path)
        if rel_dir is not None and rel_dir in catalog:
            selected.add(rel_dir)
    return sorted(selected)


def bump_changed_plugin_crates(
    workspace: Path,
    changed_paths: list[str],
    base_versions: dict[str, str] | None = None,
) -> dict[str, str]:
    workspace = workspace.resolve()
    catalog = catalog_rel_dirs(workspace)
    bumped: dict[str, str] = {}
    for rel_dir in packages_selected_for_bump(catalog, changed_paths):
        manifest = catalog[rel_dir]
        current = read_package_version(manifest)
        if base_versions is not None:
            if rel_dir not in base_versions:
                continue
            published = base_versions[rel_dir]
            if parse_xyz(current) > parse_xyz(published):
                continue
            new_version = next_publishable_version(current, published)
        else:
            new_version = bump_patch(current)
        manifest.write_text(
            replace_package_version(manifest.read_text(encoding="utf-8"), new_version),
            encoding="utf-8",
        )
        bumped[rel_dir] = new_version
    return bumped


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Bump plugin crate patches whose checksum inputs changed"
    )
    parser.add_argument(
        "--workspace",
        type=Path,
        default=None,
        help="skipprd-plugins workspace (default: sibling skipprd-plugins or SKIPPR_PLUGINS_ROOT)",
    )
    parser.add_argument(
        "--base",
        help="git ref to diff against (last engine tag). Omit when passing --changed-path",
    )
    parser.add_argument(
        "--changed-path",
        action="append",
        default=[],
        dest="changed_paths",
        help="relative path changed (repeatable; skips git when set)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print selected bumps without writing Cargo.toml",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    from plugin_workspace import resolve_plugin_workspace

    args = parse_args(argv)
    workspace = (args.workspace or resolve_plugin_workspace()).resolve()
    if args.changed_paths:
        changed_paths = [posix_path(path) for path in args.changed_paths]
        base_versions = None
    else:
        if not args.base:
            raise SystemExit("pass --base <git-ref> or --changed-path")
        changed_paths = git_changed_paths(workspace, args.base)
        catalog = catalog_rel_dirs(workspace)
        selected = packages_selected_for_bump(catalog, changed_paths)
        base_versions = {}
        for rel_dir in selected:
            version = git_base_package_version(workspace, args.base, rel_dir)
            if version is not None:
                base_versions[rel_dir] = version

    if args.dry_run:
        catalog = catalog_rel_dirs(workspace)
        selected = packages_selected_for_bump(catalog, changed_paths)
        for rel_dir in selected:
            current = read_package_version(catalog[rel_dir])
            if base_versions is not None:
                if rel_dir not in base_versions:
                    continue
                published = base_versions[rel_dir]
                if parse_xyz(current) > parse_xyz(published):
                    continue
                print(
                    f"{rel_dir}: {current} -> {next_publishable_version(current, published)}"
                )
            else:
                print(f"{rel_dir}: {current} -> {bump_patch(current)}")
        return 0

    bumped = bump_changed_plugin_crates(
        workspace, changed_paths, base_versions=base_versions
    )
    if not bumped:
        print("no plugin crate versions to bump")
        return 0
    for rel_dir, version in bumped.items():
        print(f"{rel_dir}: {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
