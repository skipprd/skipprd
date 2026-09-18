#!/usr/bin/env python3
"""Strip macOS cdylib link args from cargo build-scripts.

Maturin sets CARGO_ENCODED_RUSTFLAGS=-C link-arg=-undefined -C
link-arg=dynamic_lookup for the pyo3 cdylib. Cargo also applies those flags
to build-scripts; Darwin then fails with `cannot execute binary file`.
"""

from __future__ import annotations

import os
import sys

DROP = {"link-arg=-undefined", "link-arg=dynamic_lookup"}


def crate_name(args: list[str]) -> str | None:
    for index, arg in enumerate(args):
        if arg == "--crate-name" and index + 1 < len(args):
            return args[index + 1]
    return None


def strip_cdylib_link_args(args: list[str]) -> list[str]:
    out: list[str] = []
    index = 0
    while index < len(args):
        if args[index] == "-C" and index + 1 < len(args) and args[index + 1] in DROP:
            index += 2
            continue
        out.append(args[index])
        index += 1
    return out


def main() -> None:
    if len(sys.argv) < 2:
        sys.exit("darwin-rustc-wrapper: rustc is required")
    rustc = sys.argv[1]
    args = sys.argv[2:]
    name = crate_name(args)
    if name == "build_script_build" or (name is not None and name.startswith("build_script_")):
        args = strip_cdylib_link_args(args)
    os.execvp(rustc, [rustc, *args])


if __name__ == "__main__":
    main()
