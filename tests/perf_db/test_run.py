#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
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


def perf_db_run():
    spec = importlib.util.spec_from_file_location("perf_db_run", HARNESS_DIR / "run.py")
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


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
            self.assertIn("query_pin", phases, name)

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
        scenarios = catalog["scenarios"]
        for name in ("q-iceberg-only-iceberg", "q-iceberg-only-duckdb", "q-cdc-iceberg-only-pg"):
            self.assertEqual(scenarios[name]["asserts"].get("wal_pin"), "drained", name)
            self.assertNotIn("iceberg_only", scenarios[name], name)
        for name in ("q-stream-tail-iceberg", "q-union-overlap-duckdb", "q-cdc-overlap-pg"):
            self.assertEqual(scenarios[name]["asserts"].get("wal_pin"), "live", name)
        deleted = catalog["scenarios"]["q-cdc-union-delete-pg"]
        self.assertEqual((deleted.get("asserts") or {}).get("query_count"), 1)
        self.assertEqual((deleted.get("asserts") or {}).get("query_absent_id"), 2)


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


class PerfDbWorkflowContractTests(unittest.TestCase):
    def test_perf_db_yml_linux_matrix_lists_every_catalog_scenario(self) -> None:
        text = (REPO_ROOT / ".github" / "workflows" / "perf-db.yml").read_text(encoding="utf-8")
        matrix = text.split("\n  linux_matrix:")[1].split("\n  build_linux_profile:")[0]
        for scenario_id in scenario_ids(db_catalog()):
            self.assertIn(scenario_id, matrix, scenario_id)
        self.assertIn("actions/e2e/perf-db", text)
        self.assertIn("ubuntu-latest", text)
        self.assertNotIn("skippr-darwin-arm64", text)
        self.assertNotIn("if-no-files-found: warn", text)
        self.assertIn("if-no-files-found: error", text)
        self.assertIn("linux_contracts", matrix)
        profile_job = text.split("\n  linux_profile:")[1].split("\n  linux_analyze:")[0]
        self.assertIn("linux_contracts", profile_job)

    def test_ci_does_not_skip_docker_cdc_cells(self) -> None:
        src = (HARNESS_DIR / "run.py").read_text(encoding="utf-8")
        start = src.index("def run_query_after_cdc")
        chunk = src[start : start + 900]
        self.assertIn("GITHUB_ACTIONS", chunk)
        self.assertIn("Docker required on GitHub", chunk)
        self.assertLess(chunk.index("GITHUB_ACTIONS"), chunk.index("docker-down"))


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
        ingest = text.split("def run_query_after_ingest")[1].split("def write_cdc")[0]
        measured = ingest.find("measured_query")
        self.assertGreater(measured, 0)
        self.assertGreater(
            ingest.find("disarm_compaction_hold", measured),
            measured,
            "File overlap must hold compact until after the measured query",
        )
        self.assertIn("arm_compaction_hold", text)
        self.assertIn("OVERLAP_WAIT_SECONDS", text)
        self.assertNotIn("CLUSTER_TENANT", text)
        wal_tail = text.split('if scenario.get("wal_tail")')[1].split("else:")[0]
        self.assertIn("seconds=90", wal_tail)
        self.assertNotIn("failpoint.unlink", ingest[:measured])
        self.assertLess(
            ingest.find("require_hold_engaged"),
            measured,
            "held cells must prove the hold engaged before the measured query",
        )
        cdc = text.split("def run_query_after_cdc")[1]
        self.assertLess(cdc.find("require_hold_engaged"), cdc.find("measured_query"))
        self.assertNotIn("from tests.perf.run import", text)
        self.assertNotIn("copy_b-json-json", text)

    def test_compaction_hold_targets_local_pipeline_root(self) -> None:
        run = perf_db_run()
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = Path(tmp)
            hold = run.arm_compaction_hold(data_dir, "perf", "perf_cell")
            self.assertEqual(hold, data_dir / "perf_perf_cell" / "failpoint")
            self.assertEqual(hold.read_text(encoding="utf-8").strip(), "hold_before_compaction_sink")
            self.assertEqual(sorted(p.name for p in data_dir.rglob("failpoint")), ["failpoint"])
            run.disarm_compaction_hold(hold)
            self.assertFalse(hold.exists())

    def test_query_name_rejects_a_second_version(self) -> None:
        run = perf_db_run()
        scenario = {"asserts": {"query_name": "alpha-prime"}}
        run.assert_query_doc("cell", scenario, 0, {"header": ["name"], "rows": [["alpha-prime"]]})
        with self.assertRaisesRegex(run.perf_run.PerfError, "expected 1 row"):
            run.assert_query_doc(
                "cell",
                scenario,
                0,
                {"header": ["name"], "rows": [["alpha-prime"], ["alpha"]]},
            )

    def test_wal_pin_assert_reads_the_last_query_pin(self) -> None:
        run = perf_db_run()
        drained = {"asserts": {"wal_pin": "drained"}}
        live = {"asserts": {"wal_pin": "live"}}
        events = [
            {"phase": "query_pin", "wal_slices": 3},
            {"phase": "query_pin", "wal_slices": 0},
        ]
        run.assert_wal_pin("cell", drained, events)
        with self.assertRaisesRegex(run.perf_run.PerfError, "wal_pin live"):
            run.assert_wal_pin("cell", live, events)
        with self.assertRaisesRegex(run.perf_run.PerfError, "no query_pin"):
            run.assert_wal_pin("cell", live, [{"phase": "query_collect"}])
        run.assert_wal_pin("cell", {"asserts": {}}, [])

    def test_every_query_cell_uses_the_shared_dynamodb_store(self) -> None:
        text = (HARNESS_DIR / "run.py").read_text(encoding="utf-8")
        ingest = text.split("def run_query_after_ingest")[1].split("def write_cdc")[0]
        self.assertIn("dynamodb_store=True", ingest)
        self.assertNotIn('if sink == "skipprlake"', ingest)

    def test_hold_that_never_engaged_fails_the_cell(self) -> None:
        run = perf_db_run()
        with tempfile.TemporaryDirectory() as tmp:
            hold = run.arm_compaction_hold(Path(tmp), "perf", "perf_cell")
            exited = subprocess.Popen([sys.executable, "-c", "pass"])
            exited.wait()
            with self.assertRaisesRegex(run.perf_run.PerfError, "never engaged"):
                run.require_hold_engaged("cell", exited, hold)
            held = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
            try:
                run.require_hold_engaged("cell", held, hold)
            finally:
                held.kill()
                held.wait()

    def test_cdc_postgres_failure_includes_container_logs(self) -> None:
        text = (HARNESS_DIR / "run.py").read_text(encoding="utf-8")
        helper = text.split("def _cdc_postgres_failed")[1].split("def start_cdc_postgres")[0]
        self.assertIn("docker", helper)
        self.assertIn("logs", helper)
        start = text.split("def start_cdc_postgres")[1].split("def stop_cdc_postgres")[0]
        self.assertIn("State.Running", start)
        self.assertIn("_cdc_postgres_failed", start)

    def test_download_accepts_perf_db_workflow(self) -> None:
        download = (
            REPO_ROOT / "tests" / "perf" / "download.py"
        ).read_text(encoding="utf-8")
        self.assertIn('--workflow', download)
        self.assertIn("perf-db.yml", download)
        self.assertIn("skippr-perf-db", download)


if __name__ == "__main__":
    unittest.main()
