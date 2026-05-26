#!/usr/bin/env python3
"""Rewrite ../react path workspace deps to react-cargo registry deps for CI."""

from __future__ import annotations

import re
import sys
from pathlib import Path

REACT_PATH_DEP = re.compile(
    r'^(?P<name>react(?:-[A-Za-z0-9-]+)?) = \{ path = "\.\./react/[^"]+", version = "(?P<version>\d+\.\d+\.\d+)" \}$'
)


def main() -> int:
    cargo_toml = Path("Cargo.toml")
    if not cargo_toml.exists():
        raise SystemExit("Cargo.toml not found in workspace root")

    lines = cargo_toml.read_text(encoding="utf-8").splitlines()
    rewritten = 0
    output: list[str] = []
    for line in lines:
        match = REACT_PATH_DEP.match(line)
        if match is None:
            output.append(line)
            continue
        output.append(
            f'{match.group("name")} = {{ version = "{match.group("version")}", registry = "react-cargo" }}'
        )
        rewritten += 1

    if rewritten == 0:
        print("no ../react path dependencies found to rewrite", file=sys.stderr)
        return 1

    cargo_toml.write_text("\n".join(output) + "\n", encoding="utf-8")
    print(f"rewrote {rewritten} react workspace dependencies to react-cargo registry")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
