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
DROP_SUBSTRINGS = ("undefined", "dynamic_lookup", "install_name")


KEEP_CRATE_TYPES = {"cdylib", "dylib", "proc-macro"}


def crate_name(args: list[str]) -> str | None:
    for index, arg in enumerate(args):
        if arg == "--crate-name" and index + 1 < len(args):
            return args[index + 1]
    return None


def crate_types(args: list[str]) -> set[str]:
    types: set[str] = set()
    index = 0
    while index < len(args):
        arg = args[index]
        if arg == "--crate-type" and index + 1 < len(args):
            types.add(args[index + 1])
            index += 2
            continue
        if arg.startswith("--crate-type="):
            types.add(arg.split("=", 1)[1])
        index += 1
    return types


def strip_cdylib_link_args(args: list[str]) -> list[str]:
    out: list[str] = []
    index = 0
    while index < len(args):
        if args[index] == "-C" and index + 1 < len(args):
            value = args[index + 1]
            if value in DROP or any(part in value for part in DROP_SUBSTRINGS):
                index += 2
                continue
        out.append(args[index])
        index += 1
    return out


def main() -> None:
    if len(sys.argv) < 2:
        sys.exit("darwin-rustc-wrapper: rustc is required")
    # RUSTC_WRAPPER: wrapper rustc <args>. RUSTC: wrapper <args>.
    if os.path.basename(sys.argv[1]) in {"rustc", "rustc.exe"} or "rustc" in sys.argv[1]:
        rustc = sys.argv[1]
        args = sys.argv[2:]
    else:
        rustc = "rustc"
        args = sys.argv[1:]
    if not KEEP_CRATE_TYPES.intersection(crate_types(args)):
        args = strip_cdylib_link_args(args)
    os.execvp(rustc, [rustc, *args])


if __name__ == "__main__":
    main()
