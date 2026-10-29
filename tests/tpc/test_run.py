#!/usr/bin/env python3
"""TPC-H SHA battery is SF10 Q1/Q3/Q6 against DuckDB, DataFusion, and skipprd."""

from __future__ import annotations

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUN = (ROOT / "tests" / "tpc" / "run.py").read_text(encoding="utf-8")
WORKFLOW = (ROOT / ".github" / "workflows" / "skippr-tpc.yml").read_text(encoding="utf-8")
SKILL = (ROOT / ".cursor" / "skills" / "skippr-tpc" / "SKILL.md").read_text(encoding="utf-8")


class TpcContractTests(unittest.TestCase):
    def test_github_is_sf10_not_sf1(self) -> None:
        self.assertIn("SF10", WORKFLOW)
        self.assertIn("tpcgen-cli", WORKFLOW)
        self.assertIn("Q1", WORKFLOW)
        self.assertIn("Q3", WORKFLOW)
        self.assertIn("Q6", WORKFLOW)
        self.assertIn("duckdb", WORKFLOW.lower())
        self.assertIn("datafusion", WORKFLOW.lower())
        self.assertIn("skipprd query", WORKFLOW)
        self.assertIn("--scale 10", WORKFLOW)
        self.assertNotIn("continue-on-error", WORKFLOW)
        self.assertNotIn("-s 1", WORKFLOW)
        header = WORKFLOW.split("\njobs:", 1)[0]
        self.assertIn("workflow_dispatch:", header)
        self.assertNotIn("tags:", header)

    def test_runner_refuses_sf1_without_laptop_flag(self) -> None:
        self.assertIn("SKIPPR_TPC_LAPTOP", RUN)
        self.assertIn("scale < 10", RUN)
        self.assertIn("Q1", RUN)
        self.assertIn("Q3", RUN)
        self.assertIn("Q6", RUN)
        self.assertIn("duckdb", RUN)
        self.assertIn("datafusion", RUN)
        self.assertIn("skipprd_query", RUN)
        self.assertIn("query", RUN)
        self.assertNotIn("cloudcycle-datalake", RUN)
        self.assertNotIn("eu-west", RUN)

    def test_skill_laptop_sf1_is_not_sha_substitute(self) -> None:
        self.assertIn("name: skippr-tpc", SKILL)
        self.assertIn("SF10", SKILL)
        self.assertIn("SKIPPR_TPC_LAPTOP", SKILL)
        self.assertIn("not a sha substitute", SKILL.lower())
        self.assertNotIn("skippr-gate", SKILL)
        self.assertNotIn("cloudcycle-datalake", SKILL)
        self.assertNotIn('gh workflow run skippr-tpc.yml --ref "$(git rev-parse HEAD)"', SKILL)
        self.assertIn("git branch --show-current", SKILL)


if __name__ == "__main__":
    unittest.main()
