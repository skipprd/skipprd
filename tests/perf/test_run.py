#!/usr/bin/env python3
from __future__ import annotations

import sys
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
            ["copy-b-json-json", "ingest-a-duckdb", "ingest-a-skipprlake"],
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
        self.assertRegex(
            text,
            r"cargo build --profile profiling -p skipprd --bin skipprd[^\n]*\n\s+cargo build --profile profiling",
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
