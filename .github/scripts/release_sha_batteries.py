#!/usr/bin/env python3
"""Plan skippr-release SHA batteries: skip re-dispatch only when headSha succeeded and required jobs succeeded."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from enum import Enum
from pathlib import Path
from typing import NamedTuple

ROOT = Path(__file__).resolve().parents[2]


class Plan(str, Enum):
    SKIP_DISPATCH = "skip_dispatch"
    DISPATCH = "dispatch"
    WAIT = "wait"


class Battery(NamedTuple):
    workflow: str
    name: str
    required_jobs: tuple[str, ...] = ()


BATTERIES: tuple[Battery, ...] = (
    Battery(
        "rust.yml",
        "Rust CI/CD Pipeline",
        (
            "DST World (linux x86)",
            "File to File copy (linux x86)",
            "File to Duckdb (linux x86)",
        ),
    ),
    Battery("skippr-fuzz.yml", "skippr-fuzz"),
    Battery("skippr-tpc.yml", "skippr-tpc"),
    Battery("perf.yml", "skippr-perf"),
    Battery("perf-db.yml", "skippr-perf-db"),
)


def plan_for_runs(sha: str, runs: list[dict]) -> Plan:
    want = sha.lower()
    matched = [run for run in runs if str(run.get("headSha", "")).lower() == want]
    if not matched:
        return Plan.DISPATCH
    in_flight = [
        run
        for run in matched
        if str(run.get("status", "")) in {"queued", "in_progress", "waiting", "pending"}
    ]
    if in_flight:
        return Plan.WAIT
    latest = matched[0]
    if str(latest.get("conclusion", "")) == "success":
        return Plan.SKIP_DISPATCH
    return Plan.DISPATCH


def required_jobs_succeeded(required: tuple[str, ...], jobs: list[dict]) -> bool:
    if not required:
        return True
    by_name = {
        str(job.get("name", "")): str(job.get("conclusion", "")) for job in jobs
    }
    return all(by_name.get(name) == "success" for name in required)


def refine_skip(plan: Plan, required: tuple[str, ...], jobs: list[dict]) -> Plan:
    if plan is not Plan.SKIP_DISPATCH:
        return plan
    if required_jobs_succeeded(required, jobs):
        return Plan.SKIP_DISPATCH
    return Plan.DISPATCH


def dispatch_ref() -> str:
    """workflow_dispatch --ref is a branch or tag. GitHub rejects a commit SHA."""
    name = subprocess.check_output(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"],
        text=True,
    ).strip()
    if not name or name == "HEAD":
        raise SystemExit(
            "workflow_dispatch --ref must be a branch or tag, not a SHA; checkout a branch"
        )
    return name


def head_sha() -> str:
    return subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()


def require_sha_is_head(sha: str) -> None:
    head = head_sha()
    if head.lower() != sha.lower():
        raise SystemExit(
            f"--sha {sha} is not HEAD {head}; checkout that commit so workflow_dispatch --ref runs it"
        )


def _gh_json(args: list[str]) -> object:
    proc = subprocess.run(
        ["gh", *args],
        check=True,
        capture_output=True,
        text=True,
    )
    return json.loads(proc.stdout)


def ensure_sha(sha: str, *, repo: str = "skipprd/skipprd") -> int:
    require_sha_is_head(sha)
    failed = 0
    for battery in BATTERIES:
        runs = _gh_json(
            [
                "run",
                "list",
                "--repo",
                repo,
                "--workflow",
                battery.workflow,
                "--json",
                "conclusion,status,headSha,databaseId",
                "--limit",
                "30",
            ]
        )
        if not isinstance(runs, list):
            raise SystemExit(f"unexpected gh run list for {battery.workflow}")
        plan = plan_for_runs(sha, runs)
        if plan is Plan.SKIP_DISPATCH and battery.required_jobs:
            matched_success = [
                run
                for run in runs
                if str(run.get("headSha", "")).lower() == sha.lower()
                and str(run.get("conclusion", "")) == "success"
            ]
            if not matched_success:
                plan = Plan.DISPATCH
                print(f"{battery.workflow}: {plan.value}", file=sys.stderr)
                failed += 1
                print(
                    f"{battery.workflow} is not success on {sha}; do not tag",
                    file=sys.stderr,
                )
                continue
            latest = matched_success[0]
            viewed = _gh_json(
                [
                    "run",
                    "view",
                    str(latest["databaseId"]),
                    "--repo",
                    repo,
                    "--json",
                    "jobs",
                ]
            )
            jobs = viewed.get("jobs", []) if isinstance(viewed, dict) else []
            plan = refine_skip(plan, battery.required_jobs, jobs)
        print(f"{battery.workflow}: {plan.value}", file=sys.stderr)
        if plan is Plan.SKIP_DISPATCH:
            continue
        if plan is Plan.DISPATCH:
            subprocess.run(
                [
                    "gh",
                    "workflow",
                    "run",
                    battery.workflow,
                    "--repo",
                    repo,
                    "--ref",
                    dispatch_ref(),
                ],
                check=True,
            )
        failed += 1
        print(
            f"{battery.workflow} is not success on {sha}; do not tag",
            file=sys.stderr,
        )
    return failed


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--sha", required=True)
    parser.add_argument("--status", action="store_true")
    parser.add_argument("--ensure", action="store_true")
    args = parser.parse_args()
    if args.status == args.ensure:
        raise SystemExit("pass exactly one of --status or --ensure")
    if args.status:
        print(json.dumps([b.workflow for b in BATTERIES]))
        return
    raise SystemExit(ensure_sha(args.sha))


if __name__ == "__main__":
    main()
