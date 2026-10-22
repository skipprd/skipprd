#!/usr/bin/env python3
"""Rank flamegraph leaves against git-diff new code. Folded stacks are the authority."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
from pathlib import Path
from typing import Any

FRAME_RE = re.compile(r"^\s*(?:0x)?[0-9a-fA-F]+\s+(\S+)")
MODULE_RE = re.compile(r"\(([^)]+)\)\s*$")
SHORT_FN = 12
ADDED_FN_RE = re.compile(
    r"^\+\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)
PLUS_FILE_RE = re.compile(r"^\+\+\+ b/(.+)$")


def fold_perf_script(text: str) -> dict[str, int]:
    stacks: dict[str, int] = {}
    current: list[str] = []

    def flush() -> None:
        nonlocal current
        if not current:
            return
        key = ";".join(reversed(current))
        stacks[key] = stacks.get(key, 0) + 1
        current = []

    for line in text.splitlines():
        if not line.strip():
            flush()
            continue
        if line[:1] in {"\t", " "}:
            symbol = _frame_symbol(line)
            if not symbol:
                continue
            current.append(symbol)
            continue
        flush()
    flush()
    return stacks


def _frame_symbol(line: str) -> str | None:
    match = FRAME_RE.match(line)
    if not match:
        return None
    symbol = match.group(1).split("+", 1)[0]
    module = MODULE_RE.search(line)
    dso = Path(module.group(1)).name if module else ""
    if symbol.startswith("["):
        if dso and not dso.startswith("["):
            return dso
        return None
    return symbol


def parse_folded(text: str) -> dict[str, int]:
    stacks: dict[str, int] = {}
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        stack, sep, count_s = stripped.rpartition(" ")
        if not sep or not stack:
            raise ValueError(f"invalid folded line: {line!r}")
        stacks[stack] = stacks.get(stack, 0) + int(count_s)
    return stacks


def rust_module_needles(path: str) -> list[str]:
    posix = path.replace("\\", "/")
    if not posix.endswith(".rs"):
        return []
    if posix.startswith("src/"):
        rel = posix[len("src/") : -len(".rs")]
        parts = rel.split("/")
        if parts[-1] in {"mod", "lib", "main"}:
            parts = parts[:-1]
        if not parts:
            return ["skipprd"]
        return ["skipprd::" + "::".join(parts)]
    if posix.startswith("crates/"):
        rest = posix[len("crates/") : -len(".rs")]
        crate, _, inner = rest.partition("/src/")
        crate = crate.replace("-", "_")
        if not inner or inner in {"lib", "mod", "main"}:
            return [crate]
        parts = inner.split("/")
        if parts[-1] in {"mod", "lib", "main"}:
            parts = parts[:-1]
        if not parts:
            return [crate]
        return [crate + "::" + "::".join(parts)]
    return []


def symbols_from_diff(diff: str) -> tuple[list[str], list[str]]:
    files: list[str] = []
    seen: set[str] = set()
    functions: list[str] = []
    fn_seen: set[str] = set()
    for line in diff.splitlines():
        match = PLUS_FILE_RE.match(line)
        if match:
            path = match.group(1)
            if path == "/dev/null" or not path.endswith(".rs") or path in seen:
                continue
            seen.add(path)
            files.append(path)
            continue
        fn = ADDED_FN_RE.match(line)
        if not fn:
            continue
        name = fn.group(1)
        if name in fn_seen:
            continue
        fn_seen.add(name)
        functions.append(name)
    return files, functions


def leaf_in_new_code(symbol: str, files: list[str], functions: list[str]) -> bool:
    for path in files:
        for needle in rust_module_needles(path):
            if needle and needle in symbol:
                return True
    for name in functions:
        if len(name) < SHORT_FN:
            continue
        if symbol.endswith(f"::{name}") or f"::{name}::" in symbol:
            return True
    return False


def rank_leaves(
    stacks: dict[str, int], files: list[str], functions: list[str]
) -> list[dict[str, Any]]:
    leaves: dict[str, int] = {}
    total = sum(stacks.values())
    for stack, count in stacks.items():
        leaf = stack.rsplit(";", 1)[-1]
        leaves[leaf] = leaves.get(leaf, 0) + count
    denom = total or 1
    ranked = sorted(leaves.items(), key=lambda item: (-item[1], item[0]))
    return [
        {
            "symbol": symbol,
            "samples": samples,
            "pct": round(100.0 * samples / denom, 2),
            "in_new_code": leaf_in_new_code(symbol, files, functions),
        }
        for symbol, samples in ranked
    ]


def _sync_elapsed(payload: dict[str, Any]) -> int | None:
    events = payload.get("perf_events") or []
    elapsed = 0
    found = False
    for event in events:
        if event.get("phase") != "sync":
            continue
        found = True
        elapsed += int(event.get("elapsed_ms") or 0)
    return elapsed if found else None


def analyze_artifacts(root: Path, diff: str, *, base: str) -> dict[str, Any]:
    files, functions = symbols_from_diff(diff)
    profiles: list[dict[str, Any]] = []
    reports: list[dict[str, Any]] = []
    merged_new: dict[str, int] = {}
    merged_total = 0
    for folded in sorted(root.rglob("perf.folded")):
        stacks = parse_folded(folded.read_text(encoding="utf-8"))
        total = sum(stacks.values())
        merged_total += total
        leaves = rank_leaves(stacks, files, functions)
        for leaf in leaves:
            if leaf["in_new_code"]:
                merged_new[leaf["symbol"]] = merged_new.get(leaf["symbol"], 0) + int(
                    leaf["samples"]
                )
        profiles.append(
            {
                "path": str(folded.relative_to(root)),
                "total_samples": total,
                "hot_leaves": leaves[:20],
                "new_code": [leaf for leaf in leaves if leaf["in_new_code"]][:20],
            }
        )
    for report_path in sorted(root.rglob("perf-report.json")):
        payload = json.loads(report_path.read_text(encoding="utf-8"))
        reports.append(
            {
                "path": str(report_path.relative_to(root)),
                "scenario": payload.get("scenario"),
                "elapsed_ms": _sync_elapsed(payload) if isinstance(payload, dict) else None,
            }
        )
    denom = merged_total or 1
    hottest = sorted(merged_new.items(), key=lambda item: (-item[1], item[0]))
    return {
        "base": base,
        "changed_files": files,
        "changed_functions": functions,
        "profiles": profiles,
        "reports": reports,
        "summary": {
            "hottest_new_code": [
                {
                    "symbol": symbol,
                    "samples": samples,
                    "pct": round(100.0 * samples / denom, 2),
                }
                for symbol, samples in hottest[:20]
            ]
        },
    }


def missing_folded_message(root: Path) -> str:
    files = sorted(path.relative_to(root).as_posix() for path in root.rglob("*") if path.is_file())
    listing = "\n".join(files[:40]) if files else "(empty)"
    return f"no perf.folded under {root}\n{listing}"


def git_diff(base: str, cwd: Path) -> str:
    proc = subprocess.run(
        ["git", "diff", f"{base}...HEAD"],
        cwd=cwd,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise SystemExit(proc.stderr.strip() or f"git diff {base}...HEAD failed")
    return proc.stdout


def print_summary(report: dict[str, Any]) -> None:
    print(f"base={report['base']} changed_rs={len(report['changed_files'])}")
    hottest = report["summary"]["hottest_new_code"]
    if not hottest:
        print("no flamegraph samples matched new code; inspect overall hot_leaves")
        for profile in report["profiles"]:
            leaves = profile.get("hot_leaves") or []
            if not leaves:
                continue
            top = leaves[0]
            print(f"  {profile['path']}: {top['symbol']} {top['pct']}%")
        return
    print("hottest frames in new code:")
    for item in hottest[:10]:
        print(f"  {item['pct']:6.2f}%  {item['symbol']}  ({item['samples']} samples)")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Rank skippr-perf flamegraphs vs new code")
    parser.add_argument("--artifacts", required=True)
    parser.add_argument("--base", default="origin/main")
    parser.add_argument("--diff-file", default="")
    parser.add_argument("--out", default="bottlenecks.json")
    parser.add_argument("--repo-root", default=".")
    args = parser.parse_args(argv)
    if args.diff_file:
        diff = Path(args.diff_file).read_text(encoding="utf-8")
    else:
        diff = git_diff(args.base, Path(args.repo_root))
    report = analyze_artifacts(Path(args.artifacts), diff, base=args.base)
    Path(args.out).write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print_summary(report)
    print(f"wrote {args.out}")
    if not report["profiles"]:
        raise SystemExit(missing_folded_message(Path(args.artifacts)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
