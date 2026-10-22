#!/usr/bin/env python3
"""Download skippr-perf GitHub artifacts (flamegraphs, folded stacks, reports)."""

from __future__ import annotations

import argparse
import json
import subprocess
from pathlib import Path


def keep_artifact(name: str) -> bool:
    return name.startswith("perf-") or name.startswith("skippr-perf-") or name.startswith(
        "heaptrack-"
    )


def download_argv(
    run_id: str,
    dest: Path,
    *,
    repo: str,
    names: list[str],
) -> list[str]:
    argv = ["gh", "run", "download", str(run_id), "-D", str(dest), "-R", repo]
    kept = [name for name in names if keep_artifact(name)]
    for name in kept:
        argv.extend(["-n", name])
    return argv


def latest_run_id(branch: str, repo: str) -> str:
    proc = subprocess.run(
        [
            "gh",
            "run",
            "list",
            "--workflow",
            "perf.yml",
            "--branch",
            branch,
            "--limit",
            "1",
            "--json",
            "databaseId",
            "-R",
            repo,
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise SystemExit(proc.stderr.strip() or "gh run list failed")
    rows = json.loads(proc.stdout or "[]")
    if not rows:
        raise SystemExit(f"no skippr-perf runs on {branch}")
    return str(rows[0]["databaseId"])


def list_artifact_names(run_id: str, repo: str) -> list[str]:
    proc = subprocess.run(
        ["gh", "api", f"repos/{repo}/actions/runs/{run_id}/artifacts", "--paginate"],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        raise SystemExit(proc.stderr.strip() or "gh api artifacts failed")
    payload = json.loads(proc.stdout or "{}")
    artifacts = payload.get("artifacts") if isinstance(payload, dict) else payload
    if not isinstance(artifacts, list):
        return []
    return [str(item["name"]) for item in artifacts if item.get("name")]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Download skippr-perf GitHub artifacts")
    parser.add_argument("--run-id", default="")
    parser.add_argument("--out", default="artifacts/skippr-perf")
    parser.add_argument("--repo", default="skipprd/skipprd")
    parser.add_argument("--branch", default="")
    args = parser.parse_args(argv)
    run_id = args.run_id.strip()
    if not run_id:
        branch = args.branch.strip()
        if not branch:
            branch_proc = subprocess.run(
                ["git", "branch", "--show-current"],
                capture_output=True,
                text=True,
                check=False,
            )
            branch = (branch_proc.stdout or "").strip()
        if not branch:
            raise SystemExit("pass --run-id or --branch")
        run_id = latest_run_id(branch, args.repo)
    dest = Path(args.out)
    dest.mkdir(parents=True, exist_ok=True)
    names = list_artifact_names(run_id, args.repo)
    cmd = download_argv(run_id, dest, repo=args.repo, names=names)
    print(" ".join(cmd), flush=True)
    proc = subprocess.run(cmd, check=False)
    if proc.returncode != 0:
        raise SystemExit(proc.returncode)
    print(f"downloaded run {run_id} to {dest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
