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

    def test_ingest_unions_local_plugins_without_pipeline_flag(self) -> None:
        start = RUN.index('str(ROOT / ".github" / "scripts" / "local_runtime_plugins.py")')
        helper = RUN[start : RUN.index("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR", start)]
        self.assertNotIn("--pipeline", helper)
        yml = (ROOT / "tests" / "tpc" / "skippr.yml").read_text(encoding="utf-8")
        self.assertIn("\n  lineitem:", yml)
        self.assertIn("\n  orders:", yml)
        self.assertIn("\n  customer:", yml)
        self.assertIn("format: parquet", yml)
        self.assertNotIn("format: json", yml)
        self.assertNotIn("FORMAT JSON", RUN)
        self.assertIn("stage_parquet_source", RUN)
        self.assertIn("read_parquet", RUN)
        file_src = (ROOT / "plugins" / "data_source" / "file" / "src" / "file.rs").read_text(
            encoding="utf-8"
        )
        self.assertIn("parquet_path_to_ipc_batches", file_src)
        self.assertIn("InputFormat::Parquet", file_src)
        self.assertIn("submit_arrow_ipc_batches", file_src)
        contracts = WORKFLOW.split("\n  tpc_h_sf10:", 1)[0]
        self.assertIn("test_local_runtime_plugins.py", contracts)
        self.assertIn("test_release_sha_batteries.py", contracts)
        build = WORKFLOW.split("Build skipprd", 1)[1].split("TPC-H SF10 Q1 Q3 Q6", 1)[0]
        self.assertIn("cargo build --release -p skipprd --bin skipprd\n", build)
        self.assertIn("-p skippr-plugin-data-sink-duckdb", build)
        self.assertNotIn("--bin skipprd \\", build)
        self.assertIn(
            "rm -rf target/release/{build,deps,incremental,.fingerprint} target/debug",
            WORKFLOW,
        )

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
