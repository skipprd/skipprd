#!/usr/bin/env python3
from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

HARNESS_DIR = Path(__file__).resolve().parent
REPO_ROOT = HARNESS_DIR.parents[1]
sys.path.insert(0, str(HARNESS_DIR))

from analyze import (  # noqa: E402
    analyze_artifacts,
    fold_perf_script,
    parse_folded,
    rust_module_needles,
    symbols_from_diff,
)
from download import download_argv  # noqa: E402


PERF_SCRIPT = """\
skipprd  1001 [002]  10.001:   4000000 cycles:ppp:
\tffffffff skipprd::helpers::perf::emit_hot_path (/opt/skipprd)
\tffffffee skipprd::sync::run (/opt/skipprd)

skipprd  1001 [002]  10.002:   4000000 cycles:ppp:
\tffffffff skipprd::helpers::perf::emit_hot_path (/opt/skipprd)
\tffffffee skipprd::sync::run (/opt/skipprd)

skipprd  1001 [002]  10.003:   4000000 cycles:ppp:
\tffffffff core::ptr::drop_in_place (/opt/skipprd)
\tffffffee skipprd::sync::run (/opt/skipprd)
"""

DIFF = """\
diff --git a/src/helpers/perf.rs b/src/helpers/perf.rs
index 111..222 100644
--- a/src/helpers/perf.rs
+++ b/src/helpers/perf.rs
@@ -1,4 +1,8 @@
+pub fn emit_hot_path() {
+    let _ = 1;
+}
 fn existing() {}
"""


class FoldPerfScriptTests(unittest.TestCase):
    def test_unknown_frame_keeps_dso_basename(self) -> None:
        text = """\
skipprd  1 [000]  1.0:   1 cycles:ppp:
\tffffffff [unknown] (/opt/skipprd)
\tffffffee skipprd::sync::run (/opt/skipprd)
"""
        folded = fold_perf_script(text)
        self.assertEqual(folded, {"skipprd::sync::run;skipprd": 1})

    def test_collapses_root_first_stacks_with_counts(self) -> None:
        folded = fold_perf_script(PERF_SCRIPT)
        self.assertEqual(
            folded,
            {
                "skipprd::sync::run;skipprd::helpers::perf::emit_hot_path": 2,
                "skipprd::sync::run;core::ptr::drop_in_place": 1,
            },
        )


class ParseFoldedTests(unittest.TestCase):
    def test_parses_inferno_folded_lines(self) -> None:
        text = (
            "skipprd::sync::run;skipprd::helpers::perf::emit_hot_path 2\n"
            "skipprd::sync::run;core::ptr::drop_in_place 1\n"
        )
        stacks = parse_folded(text)
        self.assertEqual(stacks["skipprd::sync::run;skipprd::helpers::perf::emit_hot_path"], 2)
        self.assertEqual(sum(stacks.values()), 3)


class DiffNeedlesTests(unittest.TestCase):
    def test_src_rs_maps_to_skipprd_module(self) -> None:
        self.assertEqual(
            rust_module_needles("src/helpers/perf.rs"),
            ["skipprd::helpers::perf"],
        )
        self.assertEqual(rust_module_needles("src/helpers/mod.rs"), ["skipprd::helpers"])
        self.assertEqual(rust_module_needles("src/main.rs"), ["skipprd"])
        self.assertEqual(
            rust_module_needles("crates/skippr-wal-catalog/src/apply.rs"),
            ["skippr_wal_catalog::apply"],
        )

    def test_diff_collects_changed_file_and_added_fn(self) -> None:
        files, functions = symbols_from_diff(DIFF)
        self.assertEqual(files, ["src/helpers/perf.rs"])
        self.assertIn("emit_hot_path", functions)


class BottleneckTests(unittest.TestCase):
    def test_flags_hot_leaves_in_new_code(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            cell = root / "perf-ingest-a-duckdb-linux-flamegraph"
            cell.mkdir()
            (cell / "perf.folded").write_text(
                fold_perf_script(PERF_SCRIPT)
                and "\n".join(
                    f"{stack} {count}"
                    for stack, count in fold_perf_script(PERF_SCRIPT).items()
                )
                + "\n",
                encoding="utf-8",
            )
            (cell / "perf-report.json").write_text(
                json.dumps(
                    {
                        "scenario": "ingest-a-duckdb",
                        "perf_events": [{"phase": "sync", "elapsed_ms": 40}],
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            report = analyze_artifacts(root, DIFF, base="origin/main")
            self.assertEqual(report["base"], "origin/main")
            self.assertEqual(report["changed_files"], ["src/helpers/perf.rs"])
            self.assertIn("emit_hot_path", report["changed_functions"])
            hottest = report["summary"]["hottest_new_code"][0]
            self.assertIn("emit_hot_path", hottest["symbol"])
            self.assertGreaterEqual(hottest["pct"], 60.0)
            self.assertEqual(len(report["profiles"]), 1)
            self.assertTrue(report["profiles"][0]["hot_leaves"][0]["in_new_code"])
            self.assertEqual(report["reports"][0]["scenario"], "ingest-a-duckdb")

    def test_short_fn_names_do_not_flag_libc_leaves(self) -> None:
        from analyze import rank_leaves

        leaves = rank_leaves(
            {"libc;write": 10},
            ["README.md"],
            ["write", "new", "get"],
        )
        self.assertFalse(leaves[0]["in_new_code"])

    def test_overall_hot_without_matching_diff_is_not_new_code(self) -> None:
        empty_diff = "diff --git a/README.md b/README.md\n"
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            cell = root / "cell"
            cell.mkdir()
            (cell / "perf.folded").write_text(
                "skipprd::sync::run;core::ptr::drop_in_place 9\n",
                encoding="utf-8",
            )
            report = analyze_artifacts(root, empty_diff, base="HEAD")
            self.assertEqual(report["summary"]["hottest_new_code"], [])
            self.assertFalse(report["profiles"][0]["hot_leaves"][0]["in_new_code"])


class DownloadArgvTests(unittest.TestCase):
    def test_downloads_named_perf_artifacts_only(self) -> None:
        argv = download_argv(
            "99",
            Path("/tmp/out"),
            repo="skipprd/skipprd",
            names=["perf-ingest-a-duckdb-linux-flamegraph", "skipprd-linux_x86"],
        )
        self.assertEqual(
            argv[:6],
            ["gh", "run", "download", "99", "-D", "/tmp/out"],
        )
        self.assertIn("-R", argv)
        self.assertIn("skipprd/skipprd", argv)
        self.assertIn("-n", argv)
        self.assertIn("perf-ingest-a-duckdb-linux-flamegraph", argv)
        self.assertNotIn("skipprd-linux_x86", argv)


if __name__ == "__main__":
    unittest.main()
