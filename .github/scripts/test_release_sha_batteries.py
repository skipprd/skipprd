#!/usr/bin/env python3
"""SHA batteries: skip re-dispatch only when headSha succeeded and required jobs succeeded."""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / ".github" / "scripts" / "release_sha_batteries.py"
SPEC = importlib.util.spec_from_file_location("release_sha_batteries", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
mod = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(mod)


class ReleaseShaBatteriesTests(unittest.TestCase):
    def test_battery_names_are_the_five_workflows(self) -> None:
        names = [b.workflow for b in mod.BATTERIES]
        self.assertEqual(
            names,
            [
                "rust.yml",
                "skippr-fuzz.yml",
                "skippr-tpc.yml",
                "perf.yml",
                "perf-db.yml",
            ],
        )
        self.assertNotIn("skippr-gate.yml", names)
        self.assertFalse((ROOT / ".github" / "workflows" / "skippr-gate.yml").exists())

    def test_success_on_sha_skips_dispatch(self) -> None:
        runs = [
            {
                "conclusion": "success",
                "status": "completed",
                "headSha": "abc",
                "databaseId": 1,
            }
        ]
        self.assertEqual(mod.plan_for_runs("abc", runs), mod.Plan.SKIP_DISPATCH)

    def test_failure_on_sha_must_redispatch(self) -> None:
        runs = [
            {
                "conclusion": "failure",
                "status": "completed",
                "headSha": "abc",
                "databaseId": 2,
            }
        ]
        self.assertEqual(mod.plan_for_runs("abc", runs), mod.Plan.DISPATCH)

    def test_other_sha_success_does_not_count(self) -> None:
        runs = [
            {
                "conclusion": "success",
                "status": "completed",
                "headSha": "def",
                "databaseId": 3,
            }
        ]
        self.assertEqual(mod.plan_for_runs("abc", runs), mod.Plan.DISPATCH)

    def test_in_progress_on_sha_waits(self) -> None:
        runs = [
            {
                "conclusion": "",
                "status": "in_progress",
                "headSha": "abc",
                "databaseId": 4,
            }
        ]
        self.assertEqual(mod.plan_for_runs("abc", runs), mod.Plan.WAIT)

    def test_success_without_required_jobs_must_redispatch(self) -> None:
        jobs = [{"name": "cargo build (linux x86)", "conclusion": "success"}]
        self.assertEqual(
            mod.refine_skip(
                mod.Plan.SKIP_DISPATCH,
                ("DST World (linux x86)", "File to File copy (linux x86)"),
                jobs,
            ),
            mod.Plan.DISPATCH,
        )

    def test_success_with_required_jobs_skips(self) -> None:
        jobs = [
            {"name": "DST World (linux x86)", "conclusion": "success"},
            {"name": "File to File copy (linux x86)", "conclusion": "success"},
            {"name": "File to Duckdb (linux x86)", "conclusion": "success"},
        ]
        rust = next(b for b in mod.BATTERIES if b.workflow == "rust.yml")
        self.assertEqual(
            mod.refine_skip(mod.Plan.SKIP_DISPATCH, rust.required_jobs, jobs),
            mod.Plan.SKIP_DISPATCH,
        )

    def test_workflows_exist_and_never_skip_cells(self) -> None:
        for battery in mod.BATTERIES:
            path = ROOT / ".github" / "workflows" / battery.workflow
            self.assertTrue(path.is_file(), battery.workflow)
            text = path.read_text(encoding="utf-8")
            self.assertNotIn("continue-on-error", text)
            header = text.split("\njobs:", 1)[0]
            self.assertIn("workflow_dispatch:", header)

    def test_tpc_sf10_not_sf1_and_three_engines(self) -> None:
        text = (ROOT / ".github" / "workflows" / "skippr-tpc.yml").read_text(encoding="utf-8")
        self.assertIn("SF10", text)
        self.assertIn("tpcgen-cli", text)
        self.assertIn("Q1", text)
        self.assertIn("Q3", text)
        self.assertIn("Q6", text)
        self.assertIn("duckdb", text.lower())
        self.assertIn("datafusion", text.lower())
        self.assertIn("skipprd query", text)
        self.assertNotIn("continue-on-error", text)
        self.assertNotIn("-s 1", text)

    def test_fuzz_timeout_fails(self) -> None:
        text = (ROOT / ".github" / "workflows" / "skippr-fuzz.yml").read_text(encoding="utf-8")
        self.assertIn("timeout-minutes: 90", text)
        self.assertIn("linux_contracts:", text)
        self.assertIn("test_release_sha_batteries.py", text)
        self.assertIn("needs: linux_contracts", text)
        self.assertIn("cargo fuzz", text)
        self.assertIn("fuzz_corpus_has_every_named_script", text)
        seed_at = text.index("fuzz_corpus_has_every_named_script")
        fuzz_at = text.index("cargo +nightly fuzz run world_schedule")
        self.assertLess(seed_at, fuzz_at)
        self.assertNotIn(
            "cargo test -p skipprd --lib --offline -- fuzz_corpus_has_every_named_script",
            text,
        )
        self.assertIn(
            "cargo test -p skipprd --lib -- fuzz_corpus_has_every_named_script",
            text,
        )
        fuzz_toml = (ROOT / "fuzz" / "Cargo.toml").read_text(encoding="utf-8")
        self.assertIn("[patch.crates-io]", fuzz_toml)
        self.assertIn('iceberg = { path = "../third_party/iceberg" }', fuzz_toml)
        self.assertNotIn("continue-on-error", text)
        self.assertNotRegex(text, r"(?m)^          cargo fuzz run")
        self.assertEqual(text.count("cargo +nightly fuzz run"), 12)
        self.assertIn("rust-src", text)
        for target in (
            "world_schedule",
            "copy_frames",
            "failpoint_names",
            "alter_sql",
            "wal_slices",
            "catalog_row",
            "iceberg_metadata",
            "discover_json",
            "migrate_journal",
            "pipeline_yaml",
            "checkpoint_envelope",
            "order_tokens",
        ):
            self.assertTrue(
                (ROOT / "fuzz" / "fuzz_targets" / f"{target}.rs").is_file(),
                target,
            )
        self.assertTrue((ROOT / ".cursor" / "skills" / "skippr-fuzz" / "SKILL.md").is_file())
        self.assertTrue((ROOT / ".cursor" / "skills" / "skippr-tpc" / "SKILL.md").is_file())
        release = (ROOT / ".cursor" / "skills" / "skippr-release" / "SKILL.md").read_text(
            encoding="utf-8"
        )
        self.assertIn("release_sha_batteries.py", release)
        self.assertFalse((ROOT / ".cursor" / "skills" / "skippr-gate").exists())
        self.assertFalse((ROOT / ".github" / "workflows" / "skippr-gate.yml").exists())

    def test_rust_yml_has_linux_dst_not_in_linux_test_suite(self) -> None:
        text = (ROOT / ".github" / "workflows" / "rust.yml").read_text(encoding="utf-8")
        self.assertIn("linux_dst:", text)
        self.assertIn("sqlrt::dst::", text)
        linux_test = text.split("\n  linux_test_suite:", 1)[1].split("\n  linux_x86:", 1)[0]
        self.assertNotIn("cargo test -p skipprd", linux_test)
        linux_build = text.split("\n  linux_x86:", 1)[1].split("\n  macos_arm64:", 1)[0]
        self.assertNotIn("cargo test -p skipprd", linux_build)
        dst = text.split("\n  linux_dst:", 1)[1].split("\n  release_notes:", 1)[0]
        self.assertIn('run: "cargo test -p skipprd --lib -- sqlrt::dst::"', dst)
        self.assertIn("e2e_file_copy", dst)
        self.assertIn("e2e_file_duckdb", dst)
        plugins = text.split("\n  publish_runtime_plugins:", 1)[1].split("\n    steps:", 1)[0]
        self.assertIn("linux_dst", plugins)
        publish = text.split("\n  publish_skipprd:", 1)[1].split("\n    steps:", 1)[0]
        self.assertIn("linux_dst", publish)
        rust = next(b for b in mod.BATTERIES if b.workflow == "rust.yml")
        for name in rust.required_jobs:
            self.assertIn(f"name: {name}", text, name)

    def test_ensure_sha_requires_head(self) -> None:
        src = MODULE_PATH.read_text(encoding="utf-8")
        self.assertIn("def require_sha_is_head", src)
        self.assertIn('["git", "rev-parse", "HEAD"]', src)
        self.assertLess(src.index("require_sha_is_head(sha)"), src.index("for battery in BATTERIES"))
        with self.assertRaises(SystemExit) as raised:
            mod.require_sha_is_head("not-this-commit")
        self.assertIn("is not HEAD", str(raised.exception))
        mod.require_sha_is_head(mod.head_sha())

    def test_dispatch_ref_is_branch_not_sha(self) -> None:
        src = MODULE_PATH.read_text(encoding="utf-8")
        self.assertIn("def dispatch_ref", src)
        self.assertIn("--abbrev-ref", src)
        self.assertIn("dispatch_ref()", src)
        self.assertNotIn('"--ref",\n                    sha', src)
        ref = mod.dispatch_ref()
        self.assertTrue(ref)
        self.assertNotEqual(ref, "HEAD")
        self.assertNotRegex(ref, r"^[0-9a-f]{40}$")
        fuzz = (ROOT / ".cursor" / "skills" / "skippr-fuzz" / "SKILL.md").read_text(
            encoding="utf-8"
        )
        tpc = (ROOT / ".cursor" / "skills" / "skippr-tpc" / "SKILL.md").read_text(
            encoding="utf-8"
        )
        self.assertNotIn('gh workflow run skippr-fuzz.yml --ref "$(git rev-parse HEAD)"', fuzz)
        self.assertNotIn('gh workflow run skippr-tpc.yml --ref "$(git rev-parse HEAD)"', tpc)
        self.assertIn("git branch --show-current", fuzz)
        self.assertIn("git branch --show-current", tpc)
        self.assertIn("--ref this branch", fuzz)
        self.assertIn("src/sqlrt/dst/regressions/", fuzz)
        self.assertNotIn("src/sqlrt/dst/scripts.rs", fuzz)
        self.assertIn("cargo +nightly fuzz run world_schedule", fuzz)
        self.assertNotIn("\ncargo fuzz run world_schedule", fuzz)


if __name__ == "__main__":
    unittest.main()
