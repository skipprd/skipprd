#!/usr/bin/env python3
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).resolve().parent / "run.py"
SPEC = importlib.util.spec_from_file_location("skipprlake_e2e_run", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
harness = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = harness
SPEC.loader.exec_module(harness)


class SkipprLakeE2eHarnessTests(unittest.TestCase):
    def test_expected_gold_matches_twelve_shop_rows(self) -> None:
        expected = json.loads(
            (Path(__file__).resolve().parent / "expected.json").read_text(encoding="utf-8")
        )
        self.assertEqual(expected["bronze_count"], 12)
        self.assertEqual(expected["record_count"], 12)
        self.assertEqual(expected["distinct_shop_count"], 12)
        self.assertEqual(expected["amount_sum"], 780)

    def test_skippr_yml_is_file_warehouse_iceberg_ns(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "skippr.yml"
            source = Path(tmp) / "source"
            warehouse = Path(tmp) / "warehouse"
            harness.write_skippr_yml(config, source, warehouse)
            text = config.read_text(encoding="utf-8")
            self.assertIn("SkipprLake:", text)
            self.assertIn("table_namespace: bronze", text)
            self.assertIn(f"file://{warehouse}", text)
            self.assertIn("object_store:\n        type: file", text)
            self.assertNotIn("s3://", text)

    def test_dbt_fixture_is_table_materialized_without_snowflake_hooks(self) -> None:
        project = (
            Path(__file__).resolve().parent / "testdata" / "dbt" / "dbt_project.yml"
        ).read_text(encoding="utf-8")
        self.assertIn("+materialized: table", project)
        self.assertNotIn("on-run-start", project)
        self.assertNotIn("snowflake", project)
        sources = (
            Path(__file__).resolve().parent / "testdata" / "dbt" / "models" / "sources.yml"
        ).read_text(encoding="utf-8")
        self.assertIn("name: bronze", sources)
        self.assertIn("name: shop", sources)

    def test_write_source_is_twelve_rows(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "source"
            harness.write_source(source)
            files = sorted(source.glob("event_*.json"))
            self.assertEqual(len(files), 12)
            rows = [json.loads(path.read_text(encoding="utf-8")) for path in files]
            self.assertEqual(sum(row["amount"] for row in rows), 780)


if __name__ == "__main__":
    unittest.main()
