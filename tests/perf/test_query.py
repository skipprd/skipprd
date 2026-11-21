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
from matrix import (  # noqa: E402
    CDC_ALTERS,
    CDC_CASES,
    CDC_SOURCES,
    CDC_TABLES,
    query_cdc_id,
    query_cdc_migrate_id,
)

def db_catalog() -> dict:
    return load_catalog()


def perf_db_run():
    spec = importlib.util.spec_from_file_location("perf_query", HARNESS_DIR / "query.py")
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
    "q-sql-compose-duckdb",
    "q-sql-compose-stream-iceberg",
    "q-create-table-duckdb",
    "q-create-table-stream-iceberg",
    "q-migrate-rename-duckdb",
    "q-migrate-merge-duckdb",
    "q-migrate-drop-duckdb",
    "q-migrate-promote-duckdb",
    "q-migrate-rename-stream-iceberg",
    "q-migrate-merge-stream-iceberg",
    "q-migrate-drop-stream-iceberg",
    "q-migrate-promote-stream-iceberg",
) + tuple(query_cdc_id(case, source) for source in CDC_SOURCES for case in CDC_CASES) + tuple(
    query_cdc_migrate_id(alter, source) for source in CDC_SOURCES for alter in CDC_ALTERS
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
        self.assertIn("q-sql-compose-duckdb", ids)
        self.assertIn("q-migrate-rename-stream-iceberg", ids)

    def test_sql_create_table_migrate_cover_stream_and_historical(self) -> None:
        sys.path.insert(0, str(REPO_ROOT / "tests" / "perf"))
        from matrix import (  # noqa: WPS433
            ALTERS,
            INGEST_MODES,
            query_create_table_id,
            query_migrate_id,
            query_sql_id,
        )

        catalog = db_catalog()
        ids = catalog["scenarios"]
        for mode in INGEST_MODES:
            ops = ids[query_sql_id(mode)]
            aggs = ids[query_create_table_id(mode)]
            self.assertEqual(ops["kind"], "query_after_ingest")
            self.assertEqual(ops.get("transform_sql"), "compose")
            self.assertEqual(aggs.get("transform_compute"), "both")
            if mode == "stream":
                self.assertTrue(ops.get("wal_tail"))
                self.assertEqual((ops.get("asserts") or {}).get("wal_pin"), "live")
                self.assertEqual(ops["sink"], "skipprlake")
            else:
                self.assertEqual((ops.get("asserts") or {}).get("wal_pin"), "drained")
                self.assertEqual(ops["sink"], "duckdb")
            for alter in ALTERS:
                spec = ids[query_migrate_id(alter, mode)]
                self.assertEqual(spec.get("alter"), alter)
                self.assertEqual(spec["kind"], "query_after_ingest")
                self.assertTrue(spec.get("continue_ingest"), query_migrate_id(alter, mode))
                if mode == "stream":
                    self.assertTrue(spec.get("wal_tail"))
                    self.assertEqual((spec.get("asserts") or {}).get("wal_pin"), "live")
                else:
                    self.assertEqual((spec.get("asserts") or {}).get("wal_pin"), "drained")
        workflow = (REPO_ROOT / ".github" / "workflows" / "skippr-regression.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("q-sql-compose-duckdb", workflow)
        self.assertIn("q-sql-compose-stream-iceberg", workflow)
        self.assertRegex(
            workflow,
            r"- scenario: q-sql-compose-duckdb\n\s+profile: flamegraph",
        )
        self.assertRegex(
            workflow,
            r"- scenario: q-migrate-rename-stream-iceberg\n\s+profile: flamegraph",
        )

    def test_smoke_is_wal_file_iceberg_versus_duckdb(self) -> None:
        catalog = db_catalog()
        smoke = [
            name
            for name, spec in catalog["scenarios"].items()
            if spec.get("group") == "smoke"
            and spec["kind"] in {"query_after_ingest", "query_after_cdc"}
        ]
        self.assertEqual(sorted(smoke), sorted(SMOKE))
        iceberg = catalog["scenarios"]["q-wal-count-iceberg"]
        self.assertEqual(iceberg["sink"], "skipprlake")
        duckdb = catalog["scenarios"]["q-union-count-duckdb"]
        self.assertEqual(duckdb["sink"], "duckdb")

    def test_every_cell_requires_sync_and_query_collect(self) -> None:
        catalog = db_catalog()
        for name, spec in catalog["scenarios"].items():
            if spec["kind"] not in {"query_after_ingest", "query_after_cdc"}:
                continue
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

    def test_cdc_covers_every_source_case_and_migrate(self) -> None:
        catalog = db_catalog()
        ids = catalog["scenarios"]
        for source in CDC_SOURCES:
            for case in CDC_CASES:
                name = query_cdc_id(case, source)
                spec = ids[name]
                self.assertEqual(spec["kind"], "query_after_cdc", name)
                self.assertEqual(spec.get("cdc_source"), source, name)
                self.assertEqual(spec.get("cdc_case"), case, name)
                self.assertIn(CDC_TABLES[source].split(".")[1], spec["sql"], name)
                if case == "overlap":
                    self.assertTrue(spec.get("overlap"), name)
                    self.assertEqual((spec.get("asserts") or {}).get("wal_pin"), "live", name)
                if case == "iceberg-only":
                    self.assertEqual((spec.get("asserts") or {}).get("wal_pin"), "drained", name)
            for alter in CDC_ALTERS:
                name = query_cdc_migrate_id(alter, source)
                spec = ids[name]
                self.assertEqual(spec["kind"], "query_after_cdc", name)
                self.assertEqual(spec.get("cdc_source"), source, name)
                self.assertEqual(spec.get("alter"), alter, name)
                self.assertTrue(spec.get("continue_ingest"), name)
                if alter == "rename":
                    self.assertIn("display_name", spec["sql"], name)
                if alter == "promote":
                    self.assertRegex(spec["sql"], r"SELECT\s+id\b", name)
                    self.assertIn("id", (spec.get("asserts") or {}).get("query_has") or [], name)
        workflow = (REPO_ROOT / ".github" / "workflows" / "skippr-regression.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("skippr-plugin-data-source-mysql", workflow)
        self.assertIn("skippr-plugin-data-source-dynamodb", workflow)
        self.assertIn("q-cdc-union-update-mysql", workflow)
        self.assertIn("q-cdc-union-update-ddb", workflow)
        self.assertIn("q-cdc-migrate-rename-pg", workflow)


class PerfDbSkillTests(unittest.TestCase):
    def test_skill_is_query_matrix_not_ingest_copy(self) -> None:
        skill = REPO_ROOT / ".cursor" / "skills" / "skippr-regression" / "SKILL.md"
        self.assertTrue(skill.is_file(), skill)
        text = skill.read_text(encoding="utf-8")
        self.assertIn("name: skippr-regression", text)
        self.assertIn("disable-model-invocation: true", text)
        self.assertIn("Local smoke", text)
        self.assertIn("ubuntu-latest", text)
        self.assertNotIn("skippr-darwin-arm64", text)
        self.assertIn("Do **not** run skipprd-docs", text)
        self.assertIn("skippr-regression.yml", text)
        self.assertIn("q-wal-count-iceberg", text)
        self.assertIn("q-union-count-duckdb", text)
        self.assertIn("file parquet Iceberg", text)
        self.assertIn("correctness", text.lower())
        self.assertIn("heaptrack", text)
        self.assertIn("--warmup", text)
        self.assertIn("query time", text.lower())
        self.assertIn("iceberg-catalog-dynamodb", text)
        self.assertIn("tests/perf/download.py", text)
        self.assertIn("tests/perf/analyze.py", text)
        self.assertIn("tests/perf/memo.py", text)
        self.assertIn("skippr-regression.yml", text)
        self.assertIn("paul@paulhudson.me", text)
        self.assertIn("exhausted", text)
        self.assertIn("ingest-a-duckdb", text)
        self.assertNotIn("q-wal-count-file", text)
        self.assertIn("q-cdc-union-update-pg", text)
        self.assertIn("q-cdc-union-update-mysql", text)
        self.assertIn("q-cdc-union-update-ddb", text)
        self.assertIn("q-cdc-migrate-rename-pg", text)
        self.assertIn("continue ingest", text.lower())


class PerfDbWorkflowContractTests(unittest.TestCase):
    def test_perf_db_yml_linux_matrix_lists_every_catalog_scenario(self) -> None:
        text = (REPO_ROOT / ".github" / "workflows" / "skippr-regression.yml").read_text(encoding="utf-8")
        matrix = text.split("\n  linux_matrix:")[1].split("\n  build_linux_profile:")[0]
        for scenario_id in scenario_ids(db_catalog()):
            self.assertIn(scenario_id, matrix, scenario_id)
        self.assertIn("actions/e2e/regression", text)
        self.assertNotIn("actions/e2e/perf-db", text)
        self.assertIn("ubuntu-latest", text)
        self.assertNotIn("skippr-darwin-arm64", text)
        self.assertNotIn("if-no-files-found: warn", text)
        self.assertIn("if-no-files-found: error", text)
        self.assertIn("linux_contracts", matrix)
        profile_job = text.split("\n  linux_profile:")[1].split("\n  linux_analyze:")[0]
        self.assertIn("linux_contracts", profile_job)
        contracts = text.split("\n  linux_contracts:")[1].split("\n  build_linux:")[0]
        self.assertIn("tests/perf/test_run.py", contracts)
        self.assertIn("tests/perf/test_query.py", contracts)
        self.assertIn("test_release_sha_batteries.py", contracts)

    def test_linux_matrix_uses_perf_db_linux_scale(self) -> None:
        text = (REPO_ROOT / ".github" / "workflows" / "skippr-regression.yml").read_text(encoding="utf-8")
        matrix = text.split("\n  linux_matrix:")[1].split("\n  build_linux_profile:")[0]
        self.assertIn("scale: linux", matrix)
        self.assertNotIn("scale: ci", matrix)

    def test_linux_profile_apt_does_not_pin_missing_kernel_tools(self) -> None:
        text = (REPO_ROOT / ".github" / "workflows" / "skippr-regression.yml").read_text(encoding="utf-8")
        profile = text.split("\n  linux_profile:")[1].split("\n  linux_analyze:")[0]
        self.assertNotIn("linux-tools-$(uname -r)", profile)
        self.assertIn("linux-tools-generic", profile)
        self.assertIn("heaptrack", profile)

    def test_linux_analyze_runs_when_a_profile_cell_fails(self) -> None:
        text = (REPO_ROOT / ".github" / "workflows" / "skippr-regression.yml").read_text(encoding="utf-8")
        analyze = text.split("\n  linux_analyze:")[1]
        self.assertNotIn("needs.linux_profile.result == 'success'", analyze)
        self.assertIn("needs.linux_profile.result != 'skipped'", analyze)
        self.assertIn("!cancelled()", analyze)

    def test_row_count_linux_reads_perf_db_catalog(self) -> None:
        run = perf_db_run()
        catalog = db_catalog()
        scenario = catalog["scenarios"]["q-wal-count-iceberg"]
        linux_rows = int(catalog["defaults"]["linux_rows"])
        self.assertGreaterEqual(linux_rows, 10000)
        self.assertEqual(run.row_count(scenario, "linux"), linux_rows)
        self.assertEqual(run.row_count(scenario, "smoke"), int(catalog["defaults"]["smoke_rows"]))
        src = (HARNESS_DIR / "query.py").read_text(encoding="utf-8")
        self.assertNotIn("perf_run.row_count", src)

    def test_ci_does_not_skip_docker_cdc_cells(self) -> None:
        src = (HARNESS_DIR / "query.py").read_text(encoding="utf-8")
        start = src.index("def run_query_after_cdc")
        chunk = src[start : start + 900]
        self.assertIn("GITHUB_ACTIONS", chunk)
        self.assertIn("Docker required on GitHub", chunk)
        self.assertLess(chunk.index("GITHUB_ACTIONS"), chunk.index("docker-down"))


class PerfDbRunnerContractTests(unittest.TestCase):
    def test_runner_queries_after_sync_and_shares_perf_helpers(self) -> None:
        text = (HARNESS_DIR / "query.py").read_text(encoding="utf-8")
        self.assertIn("query_after_ingest", text)
        self.assertIn("query_after_cdc", text)
        self.assertIn("skipprd query", text)
        self.assertIn("--plain", text)
        self.assertIn("--warmup", text)
        self.assertIn("query_collect", text)
        self.assertIn("required_perf_for", text)
        self.assertIn("log_args(env)", text)
        self.assertIn("COMMAND_TIMEOUT_SECONDS", text)
        self.assertNotIn("perf-db-report.json", text)
        self.assertIn("perf-report.json", text)
        self.assertIn("iceberg_parquet", text)
        self.assertIn("*.parquet", text)
        self.assertIn("load_catalog", text)
        self.assertIn("timeout=120", text)
        self.assertIn("--signal=KILL", text)
        self.assertIn('CDC_DDB_TABLE = "skippr-perf-db-cdc"', text)
        self.assertIn("tables=catalog_tables", text)
        self.assertIn("MYSQL_CDC_TABLE", text)
        self.assertIn("DDB_CDC_CATALOG_TABLE", text)
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
        overlap = ingest.split('if scenario.get("overlap")')[1]
        unlink_at = overlap.find('events.json").unlink')
        second_sync = overlap.find("start_held_ingest")
        self.assertGreater(unlink_at, 0)
        self.assertGreater(second_sync, unlink_at)
        self.assertIn("arm_compaction_hold", text)
        self.assertIn("OVERLAP_WAIT_SECONDS", text)
        self.assertIn("never reached the expected query result", text)
        peek = text.split("def peek_until")[1].split("def wait_ddb_snapshot_rows")[0]
        self.assertIn("last_peek", peek)
        self.assertIn("query_plain_quiet", peek)
        self.assertNotIn("run_skipprd", peek)
        quiet = text.split("def query_plain_quiet")[1].split("def peek_until")[0]
        self.assertIn("os.killpg", quiet)
        self.assertIn("timeout=15", quiet)
        wait_snap = text.split("def wait_ddb_snapshot_rows")[1].split("def wait_log_contains")[0]
        self.assertIn("wait_iceberg_snapshot_row_count", wait_snap)
        self.assertIn("ICEBERG_SNAPSHOT_COUNT_SQL", wait_snap)
        self.assertIn("capture_output=True", wait_snap)
        self.assertNotIn("peek_until", wait_snap)
        self.assertNotIn("duckdb_scan", wait_snap)
        self.assertNotIn("wait_ddb_iceberg_newer", text)
        docker_probe = text.split("def docker_available")[1].split("def ")[0]
        self.assertIn("timeout=8", docker_probe)
        self.assertNotIn("range(5)", docker_probe)
        self.assertIn("range(3)", docker_probe)
        self.assertIn("follow ingest exited rc=", text)
        self.assertNotIn("CLUSTER_TENANT", text)
        self.assertIn("seconds=90", ingest)
        alter_tail = ingest.split('if scenario.get("alter")')[1]
        self.assertLess(
            alter_tail.find("stop_skipprd"),
            alter_tail.find("migrate"),
            "stream migrate must release the store before skipprd migrate",
        )
        held = text.split("def start_held_ingest")[1].split("def first_cell")[0]
        self.assertIn("start_new_session=True", held)
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
        setup = ingest.split("discover")[0]
        self.assertNotIn(
            "write_migration_sql",
            setup,
            "pending ALTER SQL before discover makes sync fail with Schema not found",
        )
        self.assertGreater(ingest.find("write_migration_sql"), ingest.find("discover"))
        self.assertGreater(ingest.find("write_migration_sql"), ingest.find("sync"))
        continue_at = ingest.find("continue_ingest")
        self.assertGreater(continue_at, ingest.find("write_migration_sql"))
        self.assertIn("events_b.json", ingest)
        self.assertIn("start=original_rows + 1", ingest)
        cdc = text.split("def run_query_after_cdc")[1]
        self.assertIn('scenario.get("cdc_source")', cdc)
        self.assertIn("mysql", cdc)
        self.assertIn("dynamodb", cdc.lower())
        self.assertGreater(cdc.find("write_migration_sql"), cdc.find("run_cdc_once"))
        self.assertIn("continue_ingest", cdc)

    def test_profile_query_repeats_collect_after_warmup(self) -> None:
        run = perf_db_run()
        self.assertGreaterEqual(run.PROFILE_QUERY_REPEAT, 100)
        none_cmd = run.query_plain_args(Path("skipprd"), Path("c.yml"), {}, "SELECT 1", "none")
        self.assertIn("--warmup", none_cmd)
        self.assertNotIn("--repeat", none_cmd)
        flame_cmd = run.query_plain_args(
            Path("skipprd"), Path("c.yml"), {}, "SELECT 1", "flamegraph"
        )
        self.assertIn("--repeat", flame_cmd)
        self.assertIn(str(run.PROFILE_QUERY_REPEAT), flame_cmd)
        src = (HARNESS_DIR / "query.py").read_text(encoding="utf-8")
        ingest = src.split("def run_query_after_ingest")[1].split("def write_cdc")[0]
        cdc = src.split("def run_query_after_cdc")[1]
        self.assertIn("query_plain_args(", ingest)
        self.assertIn("query_plain_args(", cdc)

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
        run.assert_query_doc(
            "cell",
            scenario,
            0,
            {"header": ["id", "name"], "rows": [["1", "alpha-prime"]]},
        )
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

    def test_every_query_cell_does_not_select_skippr_store_dynamodb(self) -> None:
        text = (HARNESS_DIR / "query.py").read_text(encoding="utf-8")
        ingest = text.split("def run_query_after_ingest")[1].split("def write_cdc")[0]
        self.assertNotIn("dynamodb_store=True", ingest)
        self.assertNotIn("type: dynamodb", ingest)
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

    def test_cdc_kill_wrap_records_sync_when_skipprd_never_emits_it(self) -> None:
        run = perf_db_run()
        captured = 'PERF_EVENT {"phase":"wal_persist","elapsed_ms":1,"rows":2,"bytes":0}\n'
        out = run.with_sync_perf(captured, 40, 2)
        phases = [ev["phase"] for ev in run.parse_perf_events(out)]
        self.assertIn("sync", phases)
        already = 'PERF_EVENT {"phase":"sync","elapsed_ms":9,"rows":2,"bytes":0}\n'
        self.assertEqual(run.with_sync_perf(already, 40, 2), already)
        src = (HARNESS_DIR / "query.py").read_text(encoding="utf-8")
        once = src.split("def run_cdc_once")[1].split("def run_query_after_cdc")[0]
        self.assertIn("with_sync_perf", once)

    def test_cdc_mysql_waits_for_sql_not_ping(self) -> None:
        text = (HARNESS_DIR / "query.py").read_text(encoding="utf-8")
        start = text.split("def start_cdc_mysql")[1].split("def stop_cdc_mysql")[0]
        self.assertIn("SELECT 1", start)
        self.assertIn("CREATE TABLE IF NOT EXISTS orders", start)
        self.assertIn("INSERT IGNORE", start)
        self.assertNotIn("mysqladmin", start)
        helper = text.split("def _mysql_cli")[1].split("def start_cdc_mysql")[0]
        self.assertIn("127.0.0.1", helper)

    def test_cdc_ddb_follows_the_stream_before_the_mutation(self) -> None:
        text = (HARNESS_DIR / "query.py").read_text(encoding="utf-8")
        run_cdc = text.split("def run_query_after_cdc")[1].split("def ", 1)[0]
        self.assertIn('source != "ddb"', run_cdc)
        follow = run_cdc.split('if source == "ddb" and case:')[-1].split("\n        else:\n")[0]
        self.assertIn("once=False", follow)
        self.assertIn("stop_logged_ingest", follow)
        self.assertIn("wait_ddb_snapshot_rows", follow)
        self.assertIn("handshake ack plugin=Dynamodb", follow)
        self.assertLess(follow.find("wait_ddb_snapshot_rows"), follow.find("apply_cdc_case"))
        self.assertLess(follow.find("handshake ack plugin=Dynamodb"), follow.find("apply_cdc_case"))
        self.assertLess(follow.find("apply_cdc_case"), follow.find("peek_until"))
        self.assertLess(
            follow.find("wait_ddb_snapshot_rows"),
            follow.find("arm_compaction_hold"),
        )
        self.assertNotIn("write_migration_sql", follow)
        self.assertGreater(run_cdc.find("write_migration_sql"), run_cdc.find("run_cdc_once"))
        self.assertIn('case and not scenario.get("alter")', run_cdc)
        stop = text.split("def stop_logged_ingest")[1].split("def ")[0]
        self.assertIn("os.killpg", stop)
        self.assertNotIn("polling shard", follow)
        self.assertNotIn("SKIPPR_RUNTIME_LOG_LEVEL", follow)
        self.assertNotIn("run_cdc_once", follow.split("wait_ddb_snapshot_rows")[0])
        self.assertIn('wal_pin") == "drained"', follow)
        self.assertIn("run_cdc_once", follow.split("wait_ddb_snapshot_rows")[1])
        self.assertIn("with_sync_perf", follow)
        self.assertGreater(
            follow.find("logs = with_sync_perf"),
            follow.find("require_hold_engaged"),
        )
        start = text.split("def start_cdc_ddb")[1].split("def stop_cdc_ddb")[0]
        self.assertIn("ddb_local.start", start)
        self.assertNotIn("amazon/dynamodb-local", start)
        self.assertIn("Docker required on GitHub", run_cdc)
        self.assertIn("_java_ddb_local_available", run_cdc)
        self.assertIn("query_sum", text.split("def cdc_query_done")[1].split("def parse_plain_query")[0])

    def test_wait_log_contains_misses_plugin_info_at_warn(self) -> None:
        run = perf_db_run()
        proc = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
        try:
            with tempfile.NamedTemporaryFile(
                "w",
                encoding="utf-8",
                suffix=".log",
                delete=False,
            ) as handle:
                handle.write("host --log perf line\nWARN skippr-plugin: idle\n")
                log = Path(handle.name)
            with self.assertRaisesRegex(run.perf_run.PerfError, "never logged 'polling shard'"):
                run.wait_log_contains(
                    log,
                    "polling shard",
                    proc,
                    seconds=1,
                    scenario_id="probe",
                )
        finally:
            proc.kill()
            proc.wait(timeout=5)
            try:
                log.unlink()
            except OSError:
                pass

    def test_perf_base_env_keeps_runtime_plugins_at_warn(self) -> None:
        run_text = (HARNESS_DIR / "run.py").read_text(encoding="utf-8")
        helper = run_text.split("def base_env")[1].split("def ")[0]
        self.assertIn('"SKIPPR_RUNTIME_LOG_LEVEL": "warn"', helper)

    def test_ddb_cdc_helpers_wait_on_snapshot_count(self) -> None:
        from ddb_cdc import ddb_snapshot_rows_ready, snapshot_count_from_doc

        self.assertEqual(snapshot_count_from_doc({"rows": [["2"]]}), 2)
        self.assertTrue(ddb_snapshot_rows_ready(2)({"rows": [["2"]]}))
        self.assertFalse(ddb_snapshot_rows_ready(2)({"rows": [["1"]]}))

    def test_ddb_default_cdc_yaml_has_no_transform_block(self) -> None:
        text = (HARNESS_DIR / "query.py").read_text(encoding="utf-8")
        writer = text.split("def write_cdc_skippr_yml")[1].split("def _cdc_postgres_failed")[0]
        ddb = writer.split('elif source == "ddb":')[1].split("else:")[0]
        self.assertIn('extra_transform = ""', ddb)
        self.assertNotIn("transform:", ddb.split("if partition_day:")[0])

    def test_query_count_one_is_not_boolean_true(self) -> None:
        run = perf_db_run()
        self.assertEqual(run.numeric_query_count(1), 1)
        self.assertIsNone(run.numeric_query_count(True))
        self.assertIsNone(run.numeric_query_count(False))
        spec = {"asserts": {"query_count": 1}}
        self.assertFalse(
            run.cdc_query_done(spec, {"header": ["n"], "rows": [["2"]]})
        )
        self.assertTrue(
            run.cdc_query_done(spec, {"header": ["n"], "rows": [["1"]]})
        )
        with self.assertRaisesRegex(run.perf_run.PerfError, "count 2 != 1"):
            run.assert_query_doc(
                "q-cdc-union-delete-ddb",
                spec,
                0,
                {"header": ["n"], "rows": [["2"]]},
            )

    def test_cdc_ddb_query_fqn_is_storage_namespace(self) -> None:
        self.assertEqual(
            CDC_TABLES["ddb"],
            "dynamodb_skipprlake_cdc.dynamodb_skippr_dynamo_cdc",
        )
        catalog = (HARNESS_DIR / "catalog.yaml").read_text(encoding="utf-8")
        self.assertIn("dynamodb_skipprlake_cdc.dynamodb_skippr_dynamo_cdc", catalog)
        self.assertNotIn("dynamodb_skipprlake_cdc.skippr_dynamo_cdc", catalog)

    def test_cdc_postgres_failure_includes_container_logs(self) -> None:
        text = (HARNESS_DIR / "query.py").read_text(encoding="utf-8")
        helper = text.split("def _cdc_postgres_failed")[1].split("def start_cdc_postgres")[0]
        self.assertIn("docker", helper)
        self.assertIn("logs", helper)
        start = text.split("def start_cdc_postgres")[1].split("def stop_cdc_postgres")[0]
        self.assertIn("State.Running", start)
        self.assertIn("_cdc_postgres_failed", start)
        self.assertIn("REPLICA IDENTITY FULL", start)

    def test_cdc_ddb_partition_yaml_uses_user_event_ts_not_synthetic(self) -> None:
        text = (HARNESS_DIR / "query.py").read_text(encoding="utf-8")
        writer = text.split("def write_cdc_skippr_yml")[1].split("def _cdc_postgres_failed")[0]
        self.assertIn("batch_time_fields: event_ts", writer)
        self.assertNotIn("batch_time_fields: _skippr_event_time", writer)
        scenarios = db_catalog()["scenarios"]
        self.assertIn("q-cdc-partition-day-ddb", scenarios)
        spec = scenarios["q-cdc-partition-day-ddb"]
        self.assertTrue(spec.get("cdc_partition_day"))
        self.assertEqual(spec.get("cdc_case"), "count")
        self.assertNotIn("skippr_event_time", spec["sql"])
        self.assertIn("SELECT *", spec["sql"])
        asserts = spec.get("asserts") or {}
        self.assertIn("event_ts", asserts.get("query_has"))
        self.assertIn("skippr_event_time", asserts.get("query_missing"))

    def test_download_accepts_perf_db_workflow(self) -> None:
        download = (
            REPO_ROOT / "tests" / "perf" / "download.py"
        ).read_text(encoding="utf-8")
        self.assertIn('--workflow', download)
        self.assertIn("skippr-regression.yml", download)
        self.assertIn("regression-", download)


if __name__ == "__main__":
    unittest.main()
