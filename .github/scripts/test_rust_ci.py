#!/usr/bin/env python3
"""GitHub-hosted Rust CI/CD must build, test, and chaos-test skipprd on linux x86."""

from __future__ import annotations

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUST = ROOT / ".github" / "workflows" / "rust.yml"
BUILD_RELEASE = ROOT / ".github" / "actions" / "rust-build-release" / "action.yaml"
RELEASE_DOCS = ROOT / "docs" / "docs" / "maintainers" / "release-workflow.md"


class RustCiTests(unittest.TestCase):
    def test_rust_workflow_exists_and_is_named(self) -> None:
        self.assertTrue(RUST.is_file(), "expected .github/workflows/rust.yml")
        self.assertEqual(RUST.name, "rust.yml")
        text = RUST.read_text(encoding="utf-8")
        self.assertTrue(text.startswith("name: Rust CI/CD Pipeline\n"))

    def test_rust_ci_builds_tests_and_chaos_on_github_linux_x86(self) -> None:
        text = RUST.read_text(encoding="utf-8")
        self.assertRegex(text, r"(?m)^    runs-on: ubuntu-latest$")
        self.assertNotRegex(text, r"(?m)^    runs-on: \[self-hosted")
        self.assertNotIn("skippr-linux-x64-16", text)
        self.assertNotIn("macos-latest", text)
        self.assertNotIn("windows-latest", text)
        self.assertNotRegex(text, r"(?m)^  darwin:")
        self.assertNotRegex(text, r"(?m)^  macos_arm64:")
        self.assertNotRegex(text, r"(?m)^  windows_x86:")
        self.assertIn("linux_test_suite:", text)
        self.assertIn("linux_x86:", text)
        self.assertIn("chaos_mode_test:", text)
        self.assertIn("publish_skipprd:", text)
        self.assertNotIn("cargo test -p skipprd", text)
        self.assertNotIn("cargo test -p skippr-lease", text)
        self.assertNotIn("cargo check --all-features", text)
        self.assertIn("test_runtime_e2e_harness.py", text)
        self.assertIn("test_local_runtime_plugins.py", text)
        self.assertIn("test_host_dependency_boundaries.py", text)
        self.assertNotIn("check_host_dependency_boundaries.py", text)
        self.assertIn("rust-build-release", text)
        self.assertIn("architecture_name: linux_x86", text)
        self.assertIn("scenario: bike_hire_many", text)
        self.assertIn("e2e/runtime_scenario", text)
        linux_test = text.split("\n  linux_test_suite:", 1)[1].split("\n  linux_x86:", 1)[0]
        self.assertIn("runs-on: ubuntu-latest", linux_test)
        self.assertNotIn("dtolnay/rust-toolchain", linux_test)
        linux_build = text.split("\n  linux_x86:", 1)[1].split("\n  chaos_mode_test:", 1)[0]
        self.assertIn("runs-on: ubuntu-latest", linux_build)
        self.assertNotIn("linux_test_suite", linux_build)
        chaos = text.split("\n  chaos_mode_test:", 1)[1].split("\n  e2e_file_duckdb:", 1)[0]
        self.assertIn("runs-on: ubuntu-latest", chaos)
        self.assertNotIn("ubuntu-latest-8-cores", chaos)
        self.assertNotIn("[self-hosted", chaos)
        self.assertIn("timeout-minutes: 240", chaos)
        self.assertIn("linux_x86", chaos)
        self.assertNotIn("- cleanup", chaos)
        self.assertNotIn("secrets.AWS_ACCESS_KEY_ID", chaos)
        self.assertNotIn("secrets.AWS_SECRET_ACCESS_KEY", chaos)
        self.assertIn("secrets.R2_ACCOUNT_ID", chaos)
        self.assertIn("secrets.R2_ACCESS_KEY_ID", chaos)
        self.assertIn("secrets.R2_SECRET_ACCESS_KEY", chaos)
        self.assertIn("e2e_file_duckdb:", text)
        self.assertIn("e2e_skipprlake:", text)
        self.assertIn("e2e_postgres_cdc:", text)
        self.assertIn("e2e_s3_schema_evolution:", text)
        self.assertIn("e2e_file_postgres:", text)
        self.assertIn("s3_skipprlake_evolve", text)
        self.assertIn("file_postgres_append", text)
        self.assertIn("skippr-plugin-data-source-s3", text)
        self.assertIn("skippr-plugin-data-source-file", text)
        self.assertIn("skippr-plugin-data-source-postgres", text)
        self.assertIn("skippr-plugin-data-sink-skipprlake", text)
        self.assertIn("skippr-plugin-data-sink-duckdb", text)
        self.assertIn("skippr-plugin-data-sink-postgres", text)
        file_duckdb = text.split("\n  e2e_file_duckdb:", 1)[1].split("\n  e2e_skipprlake:", 1)[0]
        self.assertIn("chmod +x target/release/skippr-plugin-*", file_duckdb)
        self.assertIn("unsafe_enable_version_guessing", file_duckdb)
        self.assertIn("DATA_DIR_HIGH_WATERMARK_PCT=0", file_duckdb)
        skipprlake = text.split("\n  e2e_skipprlake:", 1)[1].split("\n  e2e_postgres_cdc:", 1)[0]
        self.assertIn("chmod +x target/release/skippr-plugin-*", skipprlake)
        postgres_cdc = (
            ROOT / ".github" / "actions" / "e2e" / "postgres_skipprlake_cdc" / "action.yaml"
        ).read_text(encoding="utf-8")
        self.assertIn("UPDATE orders SET name = 'alpha-prime'", postgres_cdc)
        self.assertIn("-p 5434:5432", postgres_cdc)
        self.assertIn("SELECT 1", postgres_cdc)
        self.assertIn("unsafe_enable_version_guessing", postgres_cdc)
        self.assertIn("bronze/postgres_orders", postgres_cdc)
        self.assertIn("count(DISTINCT id)", postgres_cdc)
        self.assertIn("_skippr_order_token", postgres_cdc)
        evolve = (
            ROOT / ".github" / "actions" / "e2e" / "s3_skipprlake_evolve" / "action.yaml"
        ).read_text(encoding="utf-8")
        self.assertIn("unsafe_enable_version_guessing", evolve)
        self.assertIn("firmware_revision_decimal", evolve)
        file_postgres = (
            ROOT / ".github" / "actions" / "e2e" / "file_postgres_append" / "action.yaml"
        ).read_text(encoding="utf-8")
        self.assertIn("SELECT 1", file_postgres)
        self.assertIn("public.file_postgres_append", file_postgres)
        self.assertNotIn("public.people", file_postgres)
        self.assertNotRegex(text, r"(?m)^  cleanup:")
        publish = text.split("\n  publish_skipprd:", 1)[1]
        self.assertIn("runs-on: ubuntu-latest", publish)
        self.assertIn("chaos_mode_test", publish)
        self.assertIn("e2e_file_duckdb", publish)
        self.assertIn("e2e_skipprlake", publish)
        self.assertIn("e2e_postgres_cdc", publish)
        self.assertIn("e2e_s3_schema_evolution", publish)
        self.assertIn("e2e_file_postgres", publish)
        self.assertIn("refs/tags/", publish)
        self.assertIn("configure-r2-releases", publish)
        self.assertIn("skipprd-linux_x86.tar.gz", publish)
        self.assertIn("gh release", publish)
        self.assertIn("secrets.GITHUB_TOKEN", publish)

    def test_rust_ci_runs_only_on_engine_tags(self) -> None:
        text = RUST.read_text(encoding="utf-8")
        header = text.split("\njobs:", 1)[0]
        self.assertIn("tags:", header)
        self.assertIn('- "[0-9]*"', header)
        self.assertNotIn("branches:", header)
        self.assertNotIn("main", header)
        self.assertNotIn("master", header)
        self.assertNotIn("pull_request:", header)
        self.assertIn("workflow_dispatch:", header)

    def test_rust_build_release_accepts_skipprd_workspace_root(self) -> None:
        action = BUILD_RELEASE.read_text(encoding="utf-8")
        self.assertIn("workspace-root:", action)
        workflow = RUST.read_text(encoding="utf-8")
        self.assertIn("workspace-root: skipprd", workflow)
        self.assertIn("path: skipprd", workflow)
        self.assertIn("path: cloud", workflow)
        self.assertIn("skipprd/cloud", workflow)

    def test_release_docs_describe_github_hosted_rust_ci(self) -> None:
        text = RELEASE_DOCS.read_text(encoding="utf-8")
        self.assertIn("`.github/workflows/rust.yml`", text)
        self.assertIn("Rust CI/CD Pipeline", text)
        self.assertIn("ubuntu-latest", text)
        self.assertIn("chaos_mode_test", text)
        self.assertIn("bike_hire_many", text)
        self.assertIn("publish_skipprd", text)
        self.assertIn("engine tags", text)
        self.assertIn("not on `main`", text)
        self.assertIn("R2", text)
        self.assertIn("skipprlake", text)
        self.assertIn("file_duckdb", text)
        self.assertIn("postgres", text)
        self.assertIn("5,100,000", text)
        self.assertIn("WAL_COMPACTION_GROUP_MAX_PARTS", text)
        self.assertIn("2 MiB", text)
        self.assertIn("schema evolution", text)
        self.assertIn("Python workflow contracts", text)
        self.assertNotIn("full `cargo test -p skipprd`", text)


if __name__ == "__main__":
    unittest.main()
