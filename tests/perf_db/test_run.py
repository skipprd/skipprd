#!/usr/bin/env python3
from __future__ import annotations

import sys
import unittest
from pathlib import Path

HARNESS_DIR = Path(__file__).resolve().parent
REPO_ROOT = HARNESS_DIR.parents[1]
sys.path.insert(0, str(REPO_ROOT / "tests" / "perf"))
sys.path.insert(0, str(HARNESS_DIR))

from catalog import (  # noqa: E402
    OUT_OF_SUITE,
    load_catalog,
    required_perf_for,
    scenario_ids,
    validate_scenario_asserts,
)


SMOKE = ("q-wal-count-file", "q-wal-point-file", "q-union-count-duckdb")
REQUIRED = SMOKE + (
    "q-wal-group-file",
    "q-stream-tail-file",
    "q-union-point-duckdb",
    "q-union-group-duckdb",
    "q-iceberg-only-duckdb",
    "q-union-count-skipprlake",
)


class PerfDbCatalogTests(unittest.TestCase):
    def test_required_query_cells_exist_and_import_is_out(self) -> None:
        catalog = load_catalog()
        ids = scenario_ids(catalog)
        for name in REQUIRED:
            self.assertIn(name, ids, name)
        joined = " ".join(ids).lower()
        for banned in OUT_OF_SUITE:
            self.assertNotIn(banned, joined)
        text = (HARNESS_DIR / "catalog.yaml").read_text(encoding="utf-8").lower()
        self.assertIn("ingest then query", text)
        self.assertNotIn("cdc-postgres", joined)

    def test_smoke_group_is_wal_count_point_and_union_count(self) -> None:
        catalog = load_catalog()
        smoke = [
            name
            for name, spec in catalog["scenarios"].items()
            if spec.get("group") == "smoke"
        ]
        self.assertEqual(sorted(smoke), sorted(SMOKE))

    def test_every_cell_is_query_after_ingest_and_requires_sync_and_query_collect(self) -> None:
        catalog = load_catalog()
        for name, spec in catalog["scenarios"].items():
            self.assertEqual(spec["kind"], "query_after_ingest", name)
            phases = required_perf_for(catalog, spec)
            self.assertIn("sync", phases, name)
            self.assertIn("query_collect", phases, name)

    def test_unknown_catalog_assert_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            validate_scenario_asserts("x", {"asserts": {"min_sync_event": 12}})

    def test_query_sql_and_asserts_are_catalogued(self) -> None:
        catalog = load_catalog()
        count = catalog["scenarios"]["q-wal-count-file"]
        self.assertEqual(count["sink"], "file")
        self.assertIn("count(*)", count["sql"].lower())
        self.assertTrue((count.get("asserts") or {}).get("query_count"))
        point = catalog["scenarios"]["q-wal-point-file"]
        self.assertIn("where", point["sql"].lower())
        self.assertTrue((point.get("asserts") or {}).get("query_point"))
        stream = catalog["scenarios"]["q-stream-tail-file"]
        self.assertTrue(stream["sql"].upper().startswith("STREAM "))
        union = catalog["scenarios"]["q-union-count-duckdb"]
        self.assertEqual(union["sink"], "duckdb")
        self.assertTrue((union.get("asserts") or {}).get("query_count"))


class PerfDbSkillTests(unittest.TestCase):
    def test_skill_is_query_matrix_not_ingest_copy(self) -> None:
        skill = REPO_ROOT / ".cursor" / "skills" / "skippr-perf-db" / "SKILL.md"
        self.assertTrue(skill.is_file(), skill)
        text = skill.read_text(encoding="utf-8")
        self.assertIn("name: skippr-perf-db", text)
        self.assertIn("disable-model-invocation: true", text)
        self.assertIn("Local smoke", text)
        self.assertIn("ubuntu-latest", text)
        self.assertNotIn("skippr-darwin-arm64", text)
        self.assertIn("Do **not** run skipprd-docs", text)
        self.assertIn("perf-db.yml", text)
        self.assertIn("q-wal-count-file", text)
        self.assertIn("q-union-count-duckdb", text)
        self.assertIn("tests/perf/download.py", text)
        self.assertIn("tests/perf/analyze.py", text)
        self.assertIn("tests/perf/memo.py", text)
        self.assertIn("--workflow", text)
        self.assertIn("paul@paulhudson.me", text)
        self.assertIn("exhausted", text)
        self.assertNotIn("ingest-a-duckdb", text)


class PerfDbRunnerContractTests(unittest.TestCase):
    def test_runner_queries_after_sync_and_shares_perf_helpers(self) -> None:
        text = (HARNESS_DIR / "run.py").read_text(encoding="utf-8")
        self.assertIn("query_after_ingest", text)
        self.assertIn("skipprd query", text)
        self.assertIn("--plain", text)
        self.assertIn("query_collect", text)
        self.assertIn("required_perf_for", text)
        self.assertIn("log_args(env)", text)
        self.assertIn("COMMAND_TIMEOUT_SECONDS", text)
        self.assertIn("perf-db-report.json", text)
        self.assertNotIn("from tests.perf.run import", text)
        self.assertNotIn("copy_b-json-json", text)

    def test_download_accepts_perf_db_workflow(self) -> None:
        download = (
            REPO_ROOT / "tests" / "perf" / "download.py"
        ).read_text(encoding="utf-8")
        self.assertIn('--workflow', download)
        self.assertIn("perf-db.yml", download)
        self.assertIn("skippr-perf-db", download)


if __name__ == "__main__":
    unittest.main()
