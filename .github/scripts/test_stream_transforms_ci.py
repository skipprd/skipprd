#!/usr/bin/env python3
"""Stream-transform chaos/sim/fuzz must run on PRs against local FS Sled."""

from __future__ import annotations

import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "stream-transforms.yml"
PRECOMMIT = ROOT / "scripts" / "precommit.sh"
SMOKE = ROOT / "scripts" / "stream-transforms-smoke.sh"
RUST = ROOT / ".github" / "workflows" / "rust.yml"
CI = ROOT / ".github" / "workflows" / "ci.yml"


class StreamTransformsCiTests(unittest.TestCase):
    def test_workflow_runs_on_pull_request_without_aws(self) -> None:
        text = WORKFLOW.read_text(encoding="utf-8")
        self.assertTrue(text.startswith("name: Stream transforms\n"))
        header = text.split("jobs:", 1)[0]
        self.assertIn("pull_request:", header)
        self.assertIn("workflow_dispatch:", header)
        self.assertIn(
            'run: "cargo test -p skipprd --lib -- processor:: stream_transform::"',
            text,
        )
        self.assertIn("protobuf-compiler", text)
        self.assertIn("skipprd/cloud", text)
        self.assertNotIn("secrets.AWS_ACCESS_KEY_ID", text)
        self.assertNotIn("WAL_STORAGE=clustered", text)
        self.assertNotIn("temporary(true)", text)

    def test_release_pipelines_stay_tag_only(self) -> None:
        rust = RUST.read_text(encoding="utf-8")
        python = CI.read_text(encoding="utf-8")
        rust_header = rust.split("jobs:", 1)[0]
        python_header = python.split("jobs:", 1)[0]
        self.assertNotIn("pull_request:", rust_header)
        self.assertNotIn("pull_request:", python_header)

    def test_precommit_runs_full_lib_suite_and_contract(self) -> None:
        script = PRECOMMIT.read_text(encoding="utf-8")
        self.assertIn("cargo test -p skipprd --lib", script)
        self.assertIn("test_stream_transforms_ci.py", script)
        self.assertIn("SKIPPR_STREAM_TEST_PROFILE", script)
        smoke = SMOKE.read_text(encoding="utf-8")
        self.assertIn("SKIPPR_STREAM_TEST_PROFILE=smoke", smoke)
        self.assertIn("processor::", smoke)
        self.assertTrue(SMOKE.exists())


if __name__ == "__main__":
    unittest.main()
