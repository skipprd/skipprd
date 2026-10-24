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

CATALOG_PATH = HARNESS_DIR / "catalog.yaml"


def db_catalog() -> dict:
    return load_catalog(CATALOG_PATH)


SMOKE = (
    "q-wal-count-iceberg",
    "q-wal-point-iceberg",
    "q-union-count-duckdb",
    "q-union-overlap-duckdb",
)
REQUIRED = SMOKE + (
    "q-wal-group-iceberg",
    "q-stream-tail-iceberg",
    "q-iceberg-only-iceberg",
    "q-union-point-duckdb",
    "q-union-group-duckdb",
    "q-iceberg-only-duckdb",
    "q-cdc-union-count-pg",
    "q-cdc-union-update-pg",
    "q-cdc-union-delete-pg",
    "q-cdc-union-reinsert-pg",
    "q-cdc-union-sum-pg",
    "q-cdc-overlap-pg",
    "q-cdc-iceberg-only-pg",
)


class PerfDbCatalogTests(unittest.TestCase):
    def test_required_query_cells_exist_and_import_is_out(self) -> None:
        catalog = db_catalog()
        ids = scenario_ids(catalog)
        for name in REQUIRED:
            self.assertIn(name, ids, name)
        joined = " ".join(ids).lower()
        for banned in OUT_OF_SUITE:
            self.assertNotIn(banned, joined)
        text = (HARNESS_DIR / "catalog.yaml").read_text(encoding="utf-8").lower()
        self.assertIn("file parquet iceberg", text)
        self.assertNotIn("q-wal-count-file", ids)
        self.assertNotIn("q-union-count-skipprlake", ids)
        self.assertIn("q-cdc-union-update-pg", ids)
        self.assertIn("q-union-overlap-duckdb", ids)

    def test_smoke_is_wal_file_iceberg_versus_duckdb(self) -> None:
        catalog = db_catalog()
        smoke = [
            name
            for name, spec in catalog["scenarios"].items()
            if spec.get("group") == "smoke"
        ]
        self.assertEqual(sorted(smoke), sorted(SMOKE))
        iceberg = catalog["scenarios"]["q-wal-count-iceberg"]
        self.assertEqual(iceberg["sink"], "skipprlake")
        duckdb = catalog["scenarios"]["q-union-count-duckdb"]
        self.assertEqual(duckdb["sink"], "duckdb")

    def test_every_cell_requires_sync_and_query_collect(self) -> None:
        catalog = db_catalog()
        for name, spec in catalog["scenarios"].items():
            self.assertIn(spec["kind"], {"query_after_ingest", "query_after_cdc"}, name)
            if spec["kind"] == "query_after_ingest":
                self.assertIn(spec["sink"], {"skipprlake", "duckdb"}, name)
                self.assertTrue((spec.get("asserts") or {}).get("iceberg_parquet"), name)
            phases = required_perf_for(catalog, spec)
            self.assertIn("sync", phases, name)
            self.assertIn("query_collect", phases, name)

    def test_unknown_catalog_assert_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            validate_scenario_asserts("x", {"asserts": {"min_sync_event": 12}})

    def test_query_sql_and_asserts_are_catalogued(self) -> None:
        catalog = db_catalog()
        count = catalog["scenarios"]["q-wal-count-iceberg"]
        self.assertEqual(count["sink"], "skipprlake")
        self.assertIn("count(*)", count["sql"].lower())
        self.assertTrue((count.get("asserts") or {}).get("query_count"))
        point = catalog["scenarios"]["q-wal-point-iceberg"]
        self.assertIn("where", point["sql"].lower())
        self.assertTrue((point.get("asserts") or {}).get("query_point"))
        stream = catalog["scenarios"]["q-stream-tail-iceberg"]
        self.assertTrue(stream["sql"].upper().startswith("STREAM "))
        self.assertTrue(stream.get("wal_tail"))
        union = catalog["scenarios"]["q-union-count-duckdb"]
        self.assertEqual(union["sink"], "duckdb")
        self.assertTrue((union.get("asserts") or {}).get("query_count"))
        only_file = catalog["scenarios"]["q-iceberg-only-iceberg"]
        self.assertTrue(only_file.get("iceberg_only"))
        only_duck = catalog["scenarios"]["q-iceberg-only-duckdb"]
        self.assertTrue(only_duck.get("iceberg_only"))


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
        self.assertIn("q-wal-count-iceberg", text)
        self.assertIn("q-union-count-duckdb", text)
        self.assertIn("file parquet Iceberg", text)
        self.assertIn("correctness", text.lower())
        self.assertIn("heaptrack", text)
        self.assertIn("--warmup", text)
        self.assertIn("query time", text.lower())
        self.assertIn("offset-store-dynamodb", text)
        self.assertIn("tests/perf/download.py", text)
        self.assertIn("tests/perf/analyze.py", text)
        self.assertIn("tests/perf/memo.py", text)
        self.assertIn("--workflow", text)
        self.assertIn("paul@paulhudson.me", text)
        self.assertIn("exhausted", text)
        self.assertNotIn("ingest-a-duckdb", text)
        self.assertNotIn("q-wal-count-file", text)


class PerfDbRunnerContractTests(unittest.TestCase):
    def test_runner_queries_after_sync_and_shares_perf_helpers(self) -> None:
        text = (HARNESS_DIR / "run.py").read_text(encoding="utf-8")
        self.assertIn("query_after_ingest", text)
        self.assertIn("query_after_cdc", text)
        self.assertIn("skipprd query", text)
        self.assertIn("--plain", text)
        self.assertIn("--warmup", text)
        self.assertIn("query_collect", text)
        self.assertIn("required_perf_for", text)
        self.assertIn("log_args(env)", text)
        self.assertIn("COMMAND_TIMEOUT_SECONDS", text)
        self.assertIn("perf-db-report.json", text)
        self.assertIn("iceberg_parquet", text)
        self.assertIn("*.parquet", text)
        self.assertIn("load_perf_db_catalog", text)
        self.assertIn("timeout=120", text)
        self.assertIn("--signal=KILL", text)
        self.assertIn('CDC_DDB_TABLE = "skippr-perf-db-cdc"', text)
        self.assertIn("tables=[CDC_DDB_TABLE]", text)
        self.assertIn('scenario.get("wal_tail")', text)
        self.assertIn("stream_limit_rows", text)
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
