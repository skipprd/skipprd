#!/usr/bin/env python3
from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

HARNESS_DIR = Path(__file__).resolve().parent
REPO_ROOT = HARNESS_DIR.parents[1]
sys.path.insert(0, str(HARNESS_DIR))

from catalog import (  # noqa: E402
    OUT_OF_SUITE,
    SCENARIO_KINDS,
    load_catalog,
    required_perf_for,
    scenario_ids,
    validate_scenario_asserts,
)
from fixtures import (  # noqa: E402
    exact_arrow_record,
    is_exact_arrow_eligible_record,
    nested_flatten_record,
)
from parse import PREFIX, PerfParseError, missing_phases, parse_perf_events  # noqa: E402


class PerfCatalogTests(unittest.TestCase):
    def test_postgres_cdc_is_out_of_the_perf_matrix(self) -> None:
        ids = scenario_ids()
        self.assertNotIn("cdc-postgres-skipprlake", ids)
        workflow = (REPO_ROOT / ".github" / "workflows" / "perf.yml").read_text(encoding="utf-8")
        self.assertNotIn("cdc-postgres-skipprlake", workflow)
        self.assertNotIn("cdc_postgres_skipprlake", (HARNESS_DIR / "run.py").read_text(encoding="utf-8"))

    def test_catalog_includes_a_and_b_and_excludes_import(self) -> None:
        catalog = load_catalog()
        ids = scenario_ids(catalog)
        self.assertIn("ingest-a-duckdb", ids)
        self.assertIn("ingest-a-skipprlake", ids)
        self.assertIn("copy-b-json-json", ids)
        self.assertIn("ingest-json-parquet", ids)
        joined = " ".join(ids).lower()
        for banned in OUT_OF_SUITE:
            self.assertNotIn(banned, joined)
        text = (HARNESS_DIR / "catalog.yaml").read_text(encoding="utf-8").lower()
        self.assertIn("wal leftover import is out of suite", text)
        self.assertNotRegex(text, r"(?m)^  import:")

    def test_smoke_group_is_a_and_b(self) -> None:
        catalog = load_catalog()
        smoke = [
            name
            for name, spec in catalog["scenarios"].items()
            if spec.get("group") == "smoke"
        ]
        self.assertEqual(
            sorted(smoke),
            [
                "copy-b-json-json",
                "duckdb-csv",
                "duckdb-ndjson",
                "ingest-a-duckdb",
                "ingest-a-skipprlake",
                "ingest-csv-duckdb",
                "ingest-json-parquet",
            ],
        )

    def test_duckdb_ingest_baselines_cover_fast_and_slow_patterns(self) -> None:
        catalog = load_catalog()
        fast = catalog["scenarios"]["duckdb-ndjson"]
        csv_fast = catalog["scenarios"]["duckdb-csv"]
        nested = catalog["scenarios"]["duckdb-json-array"]
        inserts = catalog["scenarios"]["duckdb-insert-values"]
        skipprd_csv = catalog["scenarios"]["ingest-csv-duckdb"]
        self.assertEqual(fast["kind"], "duckdb_ingest")
        self.assertEqual(fast["duckdb_pattern"], "ndjson")
        self.assertEqual(csv_fast["duckdb_pattern"], "csv")
        self.assertEqual(nested["duckdb_pattern"], "json_array")
        self.assertEqual(inserts["duckdb_pattern"], "insert_values")
        self.assertEqual(skipprd_csv["kind"], "ingest")
        self.assertEqual(skipprd_csv["source_format"], "csv")
        self.assertEqual(skipprd_csv["sink"], "duckdb")
        self.assertGreaterEqual(int(fast.get("rows") or 0), 100000)
        self.assertGreaterEqual(int(csv_fast.get("rows") or 0), 100000)
        self.assertLess(int(inserts.get("rows") or 0), int(fast.get("rows") or 0))
        workflow = (REPO_ROOT / ".github" / "workflows" / "perf.yml").read_text(encoding="utf-8")
        for name in (
            "duckdb-ndjson",
            "duckdb-csv",
            "duckdb-json-array",
            "duckdb-insert-values",
            "ingest-csv-duckdb",
        ):
            self.assertIn(name, workflow)
            self.assertTrue((catalog["scenarios"][name].get("asserts") or {}).get("row_count"))

    def test_ingest_json_parquet_is_stable_schema_file_ingest(self) -> None:
        catalog = load_catalog()
        spec = catalog["scenarios"]["ingest-json-parquet"]
        self.assertEqual(spec["kind"], "ingest")
        self.assertEqual(spec["sink"], "file")
        self.assertEqual(spec["sink_format"], "parquet")
        self.assertEqual(spec["fixture"], "exact_arrow_ts")
        asserts = spec.get("asserts") or {}
        self.assertTrue(asserts.get("exact_arrow_eq_total"))
        self.assertEqual(int(asserts["exact_fallback_rows"]), 0)
        self.assertTrue(asserts.get("bronze_count"))
        self.assertTrue(asserts.get("amount_sum"))
        phases = required_perf_for(catalog, spec)
        for phase in (
            "ingest_profile",
            "plugin_ipc",
            "sync",
            "wal_persist",
            "wal_compact",
            "wal_reclaim",
        ):
            self.assertIn(phase, phases)
        workflow = (REPO_ROOT / ".github" / "workflows" / "perf.yml").read_text(encoding="utf-8")
        self.assertIn("ingest-json-parquet", workflow)
        self.assertRegex(
            workflow,
            r"- scenario: ingest-json-parquet\n\s+profile: flamegraph",
        )

    def test_every_scenario_has_typed_kind_and_requires_sync(self) -> None:
        catalog = load_catalog()
        for name, spec in catalog["scenarios"].items():
            self.assertIn(spec["kind"], SCENARIO_KINDS, name)
            phases = required_perf_for(catalog, spec)
            self.assertIn("sync", phases, name)
            self.assertTrue(phases, name)

    def test_ingest_a_requires_wal_and_exact_arrow_phases(self) -> None:
        catalog = load_catalog()
        for name in ("ingest-a-duckdb", "ingest-a-skipprlake"):
            phases = required_perf_for(catalog, catalog["scenarios"][name])
            for phase in (
                "ingest_profile",
                "plugin_ipc",
                "sync",
                "wal_persist",
                "wal_compact",
                "wal_reclaim",
            ):
                self.assertIn(phase, phases, f"{name} missing {phase}")

    def test_unknown_catalog_assert_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            validate_scenario_asserts("x", {"asserts": {"min_sync_event": 12}})

    def test_copy_identity_assert_is_catalogued(self) -> None:
        catalog = load_catalog()
        for name in ("copy-b-json-json", "copy-stdin-file", "copy-socket-file"):
            self.assertTrue((catalog["scenarios"][name].get("asserts") or {}).get("identity"), name)

    def test_wrap_cells_require_sync_and_wal_persist_per_matrix(self) -> None:
        catalog = load_catalog()
        for name in ("copy-format-matrix", "copy-s3-file", "copy-file-s3", "copy-s3-s3"):
            spec = catalog["scenarios"][name]
            phases = required_perf_for(catalog, spec)
            self.assertEqual(phases, ["sync", "wal_persist"], name)
            asserts = spec.get("asserts") or {}
            self.assertGreaterEqual(int(asserts["min_sync_events"]), 12, name)
            self.assertGreaterEqual(int(asserts["min_wal_persist_events"]), 12, name)


class PerfFixtureTests(unittest.TestCase):
    def test_exact_arrow_fixture_is_eligible(self) -> None:
        record = exact_arrow_record(1)
        self.assertTrue(is_exact_arrow_eligible_record(record, flatten=False))
        self.assertFalse(is_exact_arrow_eligible_record(record, flatten=True))

    def test_flatten_fixture_is_not_eligible(self) -> None:
        record = nested_flatten_record(1)
        self.assertFalse(is_exact_arrow_eligible_record(record, flatten=False))
        self.assertFalse(is_exact_arrow_eligible_record(record, flatten=True))
        self.assertIsInstance(record["user"], dict)

    def test_timestamp_fixture_stays_flat_and_carries_rfc3339(self) -> None:
        from fixtures import exact_arrow_ts_record, write_ndjson

        record = exact_arrow_ts_record(3)
        self.assertTrue(is_exact_arrow_eligible_record(record, flatten=False))
        self.assertEqual(record["occurred_at"], "2024-01-15T12:34:56Z")
        self.assertEqual(record["id"], 3)
        self.assertEqual(record["amount"], 30)
        with tempfile.TemporaryDirectory() as tmp:
            path = write_ndjson(Path(tmp) / "events.json", "exact_arrow_ts", 2)
            lines = path.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(lines), 2)
        self.assertIn("2024-01-15T12:34:56Z", lines[0])

    def test_csv_and_json_array_and_insert_sql_are_generated_not_checked_in(self) -> None:
        from fixtures import write_csv, write_insert_sql, write_json_array

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            csv_path = write_csv(root / "events.csv", "exact_arrow", 3)
            array_path = write_json_array(root / "events.json", "nested_flatten", 2)
            sql_path = write_insert_sql(root / "insert.sql", 2)
            csv_text = csv_path.read_text(encoding="utf-8")
            array = json.loads(array_path.read_text(encoding="utf-8"))
            sql = sql_path.read_text(encoding="utf-8")
        self.assertIn("id,name,amount", csv_text)
        self.assertIn("user_3", csv_text)
        self.assertEqual(len(array), 2)
        self.assertIsInstance(array[0]["user"], dict)
        self.assertIn("INSERT INTO events VALUES", sql)
        self.assertEqual(sql.count("INSERT INTO events VALUES"), 2)
        fixtures_dir = HARNESS_DIR / "testdata"
        self.assertFalse(fixtures_dir.exists())


class FileParquetYamlTests(unittest.TestCase):
    def test_ingest_file_sink_writes_parquet_format(self) -> None:
        from run import write_skippr_yml

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "skippr.yml"
            write_skippr_yml(
                path,
                kind="ingest",
                source_kind="File",
                source_path="/src",
                sink="file",
                warehouse=None,
                flatten=False,
                extra_source={"output_dir": "/out", "format": "parquet"},
            )
            text = path.read_text(encoding="utf-8")
        self.assertIn("format: parquet", text)
        self.assertIn("output_dir: /out", text)
        self.assertNotIn("type: copy", text)
        self.assertNotIn("schema_sink:", text)

    def test_csv_file_source_writes_csv_format(self) -> None:
        from run import write_skippr_yml

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "skippr.yml"
            write_skippr_yml(
                path,
                kind="ingest",
                source_kind="File",
                source_path="/src",
                sink="duckdb",
                warehouse=Path("/warehouse"),
                flatten=False,
                extra_source={"source_format": "csv"},
            )
            text = path.read_text(encoding="utf-8")
        self.assertIn("format: csv", text)
        self.assertIn("path: /src", text)

    def test_socket_copy_batches_ndjson_instead_of_one_line_per_ingest(self) -> None:
        from run import write_skippr_yml

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "skippr.yml"
            write_skippr_yml(
                path,
                kind="copy",
                source_kind="Socket",
                source_path="127.0.0.1:9",
                sink="file",
                warehouse=None,
                flatten=False,
                extra_source={"output_dir": "/out"},
            )
            text = path.read_text(encoding="utf-8")
        self.assertIn("mode: tcp", text)
        self.assertIn("batch_size_bytes: 1048576", text)
        self.assertIn("type: copy", text)


class StreamTransformPerfTests(unittest.TestCase):
    def test_catalog_covers_ops_and_aggregates_ingest(self) -> None:
        catalog = load_catalog()
        ops = catalog["scenarios"]["ingest-ops-duckdb"]
        aggs = catalog["scenarios"]["ingest-aggregates-duckdb"]
        self.assertEqual(ops["kind"], "ingest")
        self.assertEqual(ops["sink"], "duckdb")
        self.assertEqual(ops["group"], "always")
        self.assertTrue(ops.get("transform_ops"))
        self.assertFalse(bool(ops.get("transform_aggregates")))
        self.assertTrue((ops.get("asserts") or {}).get("exact_arrow_eq_total"))
        self.assertTrue((ops.get("asserts") or {}).get("amount_sum"))
        self.assertIn("stream_transform", required_perf_for(catalog, ops))
        self.assertEqual(aggs["kind"], "ingest")
        self.assertEqual(aggs["sink"], "duckdb")
        self.assertEqual(aggs["group"], "always")
        self.assertTrue(aggs.get("transform_aggregates"))
        self.assertFalse(bool(aggs.get("transform_ops")))
        self.assertTrue((aggs.get("asserts") or {}).get("bronze_count"))
        self.assertIn("stream_processor", required_perf_for(catalog, aggs))
        workflow = (REPO_ROOT / ".github" / "workflows" / "perf.yml").read_text(encoding="utf-8")
        self.assertIn("ingest-ops-duckdb", workflow)
        self.assertIn("ingest-aggregates-duckdb", workflow)
        self.assertRegex(
            workflow,
            r"- scenario: ingest-ops-duckdb\n\s+profile: flamegraph",
        )
        self.assertRegex(
            workflow,
            r"- scenario: ingest-aggregates-duckdb\n\s+profile: flamegraph",
        )

    def test_ingest_yaml_writes_ops_and_aggregates(self) -> None:
        from run import write_skippr_yml

        with tempfile.TemporaryDirectory() as tmp:
            ops_path = Path(tmp) / "ops.yml"
            write_skippr_yml(
                ops_path,
                kind="ingest",
                source_kind="File",
                source_path="/src",
                sink="duckdb",
                warehouse=Path("/warehouse"),
                flatten=False,
                ops=True,
            )
            aggs_path = Path(tmp) / "aggs.yml"
            write_skippr_yml(
                aggs_path,
                kind="ingest",
                source_kind="File",
                source_path="/src",
                sink="duckdb",
                warehouse=Path("/warehouse"),
                flatten=False,
                aggregates=True,
            )
            copy_path = Path(tmp) / "copy.yml"
            write_skippr_yml(
                copy_path,
                kind="copy",
                source_kind="File",
                source_path="/src",
                sink="file",
                warehouse=None,
                flatten=False,
                extra_source={"output_dir": "/out"},
            )
            ops_text = ops_path.read_text(encoding="utf-8")
            aggs_text = aggs_path.read_text(encoding="utf-8")
            copy_text = copy_path.read_text(encoding="utf-8")
        self.assertIn('filter: "amount > 0"', ops_text)
        self.assertIn("project: [id, name, amount]", ops_text)
        self.assertNotIn("derive:", ops_text)
        self.assertNotIn("aggregates:", ops_text)
        self.assertIn("name_totals", aggs_text)
        self.assertIn("key_fields: [name]", aggs_text)
        self.assertIn("{ sum: amount }", aggs_text)
        self.assertNotIn("ops:", aggs_text)
        self.assertNotIn("ops:", copy_text)
        self.assertNotIn("aggregates:", copy_text)

    def test_skill_smoke_includes_stream_transform_cells(self) -> None:
        skill = (REPO_ROOT / ".cursor" / "skills" / "skippr-perf" / "SKILL.md").read_text(
            encoding="utf-8"
        )
        self.assertIn("ingest-ops-duckdb", skill)
        self.assertIn("ingest-aggregates-duckdb", skill)


class PerfParseTests(unittest.TestCase):
    def test_parses_perf_event_lines_and_detects_missing_phases(self) -> None:
        log = """
2026-10-22 INFO PERF_EVENT {"phase":"ingest_profile","elapsed_ms":12,"rows":10,"exact_arrow_rows":10,"exact_fallback_rows":0}
noise
PERF_EVENT {"phase":"sync","elapsed_ms":40,"rows":10,"bytes":100}
"""
        events = parse_perf_events(log)
        self.assertEqual([event["phase"] for event in events], ["ingest_profile", "sync"])
        self.assertEqual(missing_phases(events, ["ingest_profile", "sync", "plugin_ipc"]), ["plugin_ipc"])

    def test_invalid_perf_event_json_fails(self) -> None:
        with self.assertRaises(PerfParseError):
            parse_perf_events("PERF_EVENT {not-json")

    def test_perf_event_missing_phase_fails(self) -> None:
        with self.assertRaises(PerfParseError):
            parse_perf_events('PERF_EVENT {"rows": 1}')

    def test_perf_event_prefix_matches_rust(self) -> None:
        rust = (REPO_ROOT / "src" / "helpers" / "perf.rs").read_text(encoding="utf-8")
        self.assertIn(f'PREFIX: &str = "{PREFIX}"', rust)


class PerfSkillTests(unittest.TestCase):
    def test_skill_mentions_smoke_first_custom_runner_and_no_docs(self) -> None:
        skill = REPO_ROOT / ".cursor" / "skills" / "skippr-perf" / "SKILL.md"
        self.assertTrue(skill.is_file(), skill)
        text = skill.read_text(encoding="utf-8")
        self.assertIn("name: skippr-perf", text)
        self.assertIn("disable-model-invocation: true", text)
        self.assertIn("Local smoke", text)
        self.assertIn("ubuntu-latest", text)
        self.assertNotIn("skippr-darwin-arm64", text)
        self.assertIn("Do **not** run skipprd-docs", text)
        self.assertIn("aws s3 sync --delete", text)
        self.assertIn("--log perf", text)
        self.assertIn("tests/perf/download.py", text)
        self.assertIn("tests/perf/analyze.py", text)
        self.assertIn("tests/perf/memo.py", text)
        self.assertIn("new code", text)
        self.assertIn("perf-stat", text)
        self.assertIn("paul@paulhudson.me", text)
        self.assertIn("send-afk-mail.py", text)
        self.assertIn("iterate", text)
        self.assertIn("exhausted", text)
        self.assertIn("duckdb-ndjson", text)
        self.assertIn("duckdb-csv", text)
        self.assertIn("ingest-csv-duckdb", text)


class SkipprdCliTests(unittest.TestCase):
    def test_log_args_reads_skippr_log_level(self) -> None:
        sys.path.insert(0, str(REPO_ROOT / ".github" / "scripts"))
        from skipprd_cli import log_args

        self.assertEqual(log_args({"SKIPPR_LOG_LEVEL": "perf"}), ["--log", "perf"])
        self.assertEqual(log_args({}), ["--log", "info"])
        self.assertEqual(log_args({"SKIPPR_LOG_LEVEL": "  "}), ["--log", "info"])


class PerfRunnerContractTests(unittest.TestCase):
    def test_runner_uses_typed_kind_and_required_perf_for(self) -> None:
        text = (HARNESS_DIR / "run.py").read_text(encoding="utf-8")
        self.assertIn("required_perf_for", text)
        self.assertIn("scenario_kind", text)
        self.assertIn("assert_scenario_perf", text)
        self.assertIn("assert_copy_identity", text)
        self.assertIn("log_args(env)", text)
        self.assertIn("perf-stat", text)
        self.assertIn("require_profile_outputs", text)
        self.assertIn("mode: line", text)
        self.assertIn("stdin_data", text)
        host = (REPO_ROOT / "src" / "runtime_plugins" / "host.rs").read_text(encoding="utf-8")
        self.assertIn("RuntimePluginStdin::Inherit", host)
        self.assertIn("source_capabilities::STDIN", host)
        self.assertIn("wrap_skippr_yml", text)
        self.assertIn("COMMAND_TIMEOUT_SECONDS", text)
        self.assertNotIn("run_cdc", text)
        self.assertIn("run_duckdb_ingest", text)
        self.assertIn("duckdb_ingest", text)
        self.assertNotIn('list(scenario.get("required_perf") or [])', text)
        self.assertNotIn("collect_log_text", text)
        self.assertNotIn('"--log", "perf"', text)

    def test_catalog_wrap_scripts_use_shared_log_args(self) -> None:
        catalog = load_catalog()
        for name, spec in catalog["scenarios"].items():
            if spec.get("kind") != "wrap_existing":
                continue
            path = REPO_ROOT / str(spec["script"])
            text = path.read_text(encoding="utf-8")
            self.assertIn("from skipprd_cli import log_args", text, name)
            self.assertIn("*log_args(env)", text, name)
            self.assertNotRegex(text, r'"--log",\s*"--pipeline"', name)


class WrapSkipprYmlTests(unittest.TestCase):
    def test_file_to_s3_uses_sink_yaml(self) -> None:
        from run import wrap_skippr_yml

        sink = wrap_skippr_yml(
            REPO_ROOT / ".github" / "actions" / "e2e" / "s3_file_copy" / "run_s3_sink.py"
        )
        self.assertEqual(sink.name, "skippr_s3_sink.yml")
        self.assertTrue(sink.is_file(), sink)
        self.assertIn("file_s3_copy:", sink.read_text(encoding="utf-8"))

    def test_s3_to_file_keeps_skippr_yml(self) -> None:
        from run import wrap_skippr_yml

        yml = wrap_skippr_yml(
            REPO_ROOT / ".github" / "actions" / "e2e" / "s3_file_copy" / "run_matrix.py"
        )
        self.assertEqual(yml.name, "skippr.yml")
        self.assertIn("s3_file_copy:", yml.read_text(encoding="utf-8"))


class CommandTimeoutTests(unittest.TestCase):
    def test_run_logged_kills_hung_commands(self) -> None:
        import os

        from run import COMMAND_TIMEOUT_SECONDS, PerfError, run_logged

        self.assertEqual(COMMAND_TIMEOUT_SECONDS, 600)
        with self.assertRaises(PerfError) as ctx:
            run_logged(["sleep", "30"], env=os.environ.copy(), timeout=1)
        self.assertIn("timed out", str(ctx.exception).lower())


class MemoTests(unittest.TestCase):
    def test_memo_is_plain_english_and_names_next_ideas(self) -> None:
        from memo import MEMO_TO, ideas_from_report, write_memo

        self.assertEqual(MEMO_TO, "paul@paulhudson.me")
        report = {
            "profiles": [
                {
                    "path": "ingest-a/perf.folded",
                    "hot_leaves": [
                        {"symbol": "sha2::sha256::x86::digest_blocks", "pct": 59.23},
                        {"symbol": "read", "pct": 38.44},
                    ],
                }
            ],
            "summary": {"hottest_new_code": [{"symbol": "import_if_needed", "pct": 0.0}]},
        }
        reports = {
            "ingest-a-duckdb": {
                "scenario": "ingest-a-duckdb",
                "elapsed_ms": 403,
                "rows": 10000,
                "exact_arrow_rows": 10000,
            },
            "copy-b-json-json": {
                "scenario": "copy-b-json-json",
                "elapsed_ms": 152,
                "rows": 10000,
            },
            "copy-file-s3": {
                "scenario": "copy-file-s3",
                "error": "pipeline 'file_s3_copy' was not found",
            },
            "copy-stdin-file": {
                "scenario": "copy-stdin-file",
                "error": "missing PERF phases ['wal_persist']",
            },
        }
        ideas = ideas_from_report(report, reports)
        self.assertTrue(any("SHA-256" in idea or "sha2" in idea.lower() for idea in ideas))
        self.assertTrue(any("copy-file-s3" in idea for idea in ideas))
        body = write_memo(
            iteration=1,
            run_url="https://github.com/skipprd/skipprd/actions/runs/1",
            timings=["Ingest-A synced 10,000 rows in 403ms.", "Copy-B synced 10,000 rows in 152ms."],
            broke=["File-to-S3 used the wrong config, so the pipeline name did not match."],
            done=["Cap skipprd at ten minutes and kill the process group."],
            nxt=["See whether we hash each ingest batch more than once."],
        )
        self.assertIn("Iteration 1", body)
        self.assertIn("paul@paulhudson.me", body)
        self.assertNotIn("|", body)
        self.assertNotIn("```", body)
        self.assertIn("403ms", body)
        self.assertIn("ten minutes", body)
        self.assertIn("https://github.com/skipprd/skipprd/actions/runs/1", body)
        from memo import broke_from_reports

        broke_lines = broke_from_reports(reports)
        self.assertTrue(any("pipeline name" in line for line in broke_lines))
        self.assertFalse(any("hostedtoolcache" in line for line in broke_lines))

    def test_query_cell_timings_name_file_iceberg_and_duckdb(self) -> None:
        from memo import timings_from_reports

        lines = timings_from_reports(
            {
                "q-wal-count-iceberg": {
                    "scenario": "q-wal-count-iceberg",
                    "kind": "query_after_ingest",
                    "sink": "skipprlake",
                    "rows": 200,
                    "query_collect_ms": 17,
                    "elapsed_ms": 17,
                },
                "q-union-count-duckdb": {
                    "scenario": "q-union-count-duckdb",
                    "kind": "query_after_ingest",
                    "sink": "duckdb",
                    "rows": 200,
                    "query_collect_ms": 9,
                    "elapsed_ms": 9,
                },
            }
        )
        joined = " ".join(lines)
        self.assertIn("WAL+file Iceberg count", joined)
        self.assertIn("DuckDB UNION count", joined)
        self.assertIn("17ms", joined)
        self.assertIn("9ms", joined)


class WrapProfileTests(unittest.TestCase):
    def test_perf_stat_writes_perf_stat_txt(self) -> None:
        from run import wrap_profile

        work = Path("/tmp/skippr-perf-cell")
        with (
            patch("run.shutil.which", return_value="/usr/bin/perf"),
            patch("run.subprocess.run"),
        ):
            cmd = wrap_profile(["skipprd", "sync"], "perf-stat", work)
        self.assertEqual(
            cmd,
            ["/usr/bin/perf", "stat", "-o", str(work / "perf-stat.txt"), "--", "skipprd", "sync"],
        )


class PerfWorkflowContractTests(unittest.TestCase):
    def test_perf_yml_lists_every_catalog_scenario_and_one_runner(self) -> None:
        text = (REPO_ROOT / ".github" / "workflows" / "perf.yml").read_text(encoding="utf-8")
        for scenario_id in scenario_ids():
            self.assertIn(scenario_id, text)
        self.assertIn("actions/e2e/perf", text)
        self.assertIn("actions/e2e/heaptrack", text)
        self.assertNotIn("python3 tests/perf/run.py", text)
        self.assertIn("ubuntu-latest", text)
        self.assertNotIn("skippr-darwin-arm64", text)
        self.assertNotIn("[self-hosted", text)
        self.assertIn("linux_x86", text)
        self.assertIn("perf-stat", text)
        self.assertIn("linux_analyze", text)
        self.assertIn("build_linux_profile", text)
        self.assertIn("perf.folded", text)
        self.assertIn("needs.linux_profile.result == 'success'", text)
        self.assertIn("perf-*-linux-flamegraph", text)
        self.assertNotIn("if: always() && needs.linux_profile.result != 'skipped'", text)
        self.assertIn("ingest-slow-flatten-duckdb", text)
        self.assertIn("analyze.py", text)
        self.assertIn("python3 -m pip install --quiet boto3", text)
        self.assertIn("-p skippr-plugin-data-source-file", text)
        self.assertIn('ln -sfn "$host" "$plugins/.host"', text)
        self.assertIn('CARGO_TARGET_DIR="$host/target"', text)
        self.assertRegex(
            text,
            r'\(cd "\$host" && cargo build --profile profiling -p skipprd --bin skipprd[^\n]*\n'
            r'\s+\(cd "\$plugins" && cargo build --profile profiling',
        )
        self.assertIn("target/profiling/", text)
        self.assertNotIn("if-no-files-found: warn", text)
        self.assertIn("if-no-files-found: error", text)
        cell = (REPO_ROOT / ".github" / "scripts" / "run_perf_cell.sh").read_text(encoding="utf-8")
        self.assertIn("tests/perf/run.py", cell)
        perf_action = (REPO_ROOT / ".github" / "actions" / "e2e" / "perf" / "action.yaml").read_text(
            encoding="utf-8"
        )
        heaptrack = (
            REPO_ROOT / ".github" / "actions" / "e2e" / "heaptrack" / "action.yaml"
        ).read_text(encoding="utf-8")
        self.assertIn("run_perf_cell.sh", perf_action)
        self.assertIn("run_perf_cell.sh", heaptrack)
        self.assertNotIn("python3 tests/perf/run.py", perf_action)
        self.assertNotIn("python3 tests/perf/run.py", heaptrack)
        self.assertIn("heaptrack produced no profile", heaptrack)
        self.assertIn("if-no-files-found: error", heaptrack)
        self.assertNotIn("if-no-files-found: warn", heaptrack)


if __name__ == "__main__":
    unittest.main()
