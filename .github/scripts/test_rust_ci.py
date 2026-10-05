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
        self.assertIn("cargo test -p skipprd -- --nocapture --test-threads=1", text)
        self.assertIn("check_host_dependency_boundaries.py", text)
        self.assertIn("test_runtime_e2e_harness.py", text)
        self.assertIn("rust-build-release", text)
        self.assertIn("architecture_name: linux_x86", text)
        self.assertIn("scenario: bike_hire_many", text)
        self.assertIn("e2e/runtime_scenario", text)
        linux_test = text.split("\n  linux_test_suite:", 1)[1].split("\n  linux_x86:", 1)[0]
        self.assertIn("runs-on: ubuntu-latest", linux_test)
        linux_build = text.split("\n  linux_x86:", 1)[1].split("\n  chaos_mode_test:", 1)[0]
        self.assertIn("runs-on: ubuntu-latest", linux_build)
        self.assertIn("needs:", linux_build)
        self.assertIn("linux_test_suite", linux_build)
        chaos = text.split("\n  chaos_mode_test:", 1)[1].split("\n  cleanup:", 1)[0]
        self.assertIn("runs-on: ubuntu-latest", chaos)
        self.assertIn("linux_x86", chaos)
        self.assertIn("secrets.AWS_ACCESS_KEY_ID", chaos)
        self.assertIn("secrets.AWS_SECRET_ACCESS_KEY", chaos)
        publish = text.split("\n  publish_skipprd:", 1)[1]
        self.assertIn("runs-on: ubuntu-latest", publish)
        self.assertIn("chaos_mode_test", publish)
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


if __name__ == "__main__":
    unittest.main()
