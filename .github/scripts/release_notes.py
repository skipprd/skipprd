#!/usr/bin/env python3
"""Print the CHANGELOG.md section for a release tag. A tag without a non-empty
`## [<tag>]` section fails, so a release never ships empty notes."""

from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def section(changelog: str, tag: str) -> str:
    heading = f"## [{tag}]"
    lines = changelog.splitlines()
    start = next(
        (i for i, line in enumerate(lines) if line == heading or line.startswith(heading + " ")),
        None,
    )
    if start is None:
        raise SystemExit(f"CHANGELOG.md has no '{heading}' section")
    end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")), len(lines))
    body = "\n".join(lines[start + 1 : end]).strip()
    if not body:
        raise SystemExit(f"CHANGELOG.md '{heading}' section is empty")
    return body + "\n"


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: release_notes.py <tag>")
    sys.stdout.write(section((ROOT / "CHANGELOG.md").read_text(encoding="utf-8"), sys.argv[1]))


if __name__ == "__main__":
    main()
