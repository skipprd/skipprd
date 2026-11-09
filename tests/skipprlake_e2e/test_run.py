#!/usr/bin/env python3
import importlib.util
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

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
        self.assertEqual(expected["day_2026_11_07"], 6)
        self.assertEqual(expected["day_2026_11_08"], 6)
        self.assertEqual(expected["day_missing"], 0)

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
            self.assertIn(f"catalog_table: {harness.STORE_TABLE}", text)
            self.assertNotIn("store:\n    type: dynamodb", text)
            self.assertNotIn("s3://", text)

    def test_skippr_yml_sets_iceberg_day_time_partitions(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "skippr.yml"
            harness.write_skippr_yml(config, Path(tmp) / "source", Path(tmp) / "warehouse")
            text = config.read_text(encoding="utf-8")
            self.assertIn("batch_time_fields: event_ts", text)
            self.assertIn("batch_time_unit: day", text)
            self.assertNotIn("time_partition_prefix", text)

    def test_date_partition_sql_is_hive_parity_year_month_day(self) -> None:
        self.assertEqual(
            harness.date_partition_sql("bronze.shop", 2026, 11, 7),
            "SELECT count(*) FROM bronze.shop WHERE year = 2026 AND month = 11 AND day = 7",
        )

    def test_assert_iceberg_date_partitions_checks_both_days_and_missing(self) -> None:
        seen: list[str] = []

        def count_sql(sql: str) -> int:
            seen.append(sql)
            if "day = 7" in sql:
                return 6
            if "day = 8" in sql:
                return 6
            if "day = 9" in sql:
                return 0
            raise AssertionError(sql)

        harness.assert_iceberg_date_partitions(count_sql, "bronze.shop")
        self.assertEqual(len(seen), 3)
        self.assertTrue(all("year = 2026" in sql and "month = 11" in sql for sql in seen))

    def test_assert_iceberg_amount_sum_checks_source_amounts(self) -> None:
        seen: list[str] = []

        def count_sql(sql: str) -> int:
            seen.append(sql)
            return 780

        harness.assert_iceberg_amount_sum(count_sql, "bronze.shop")
        self.assertEqual(seen, ["SELECT sum(amount) FROM bronze.shop"])

    def test_assert_iceberg_amount_sum_fails_when_id_values_land_in_amount(self) -> None:
        def count_sql(sql: str) -> int:
            del sql
            return 78

        with self.assertRaises(harness.HarnessError) as ctx:
            harness.assert_iceberg_amount_sum(count_sql, "shop.shop")
        self.assertIn("78 != 780", str(ctx.exception))
        self.assertIn("sum(amount)", str(ctx.exception))

    def test_assert_iceberg_date_partitions_fails_when_columns_missing(self) -> None:
        def count_sql(sql: str) -> int:
            raise harness.HarnessError(f"COLUMN_NOT_FOUND: {sql}")

        with self.assertRaises(harness.HarnessError) as ctx:
            harness.assert_iceberg_date_partitions(count_sql, "bronze.shop")
        self.assertIn("COLUMN_NOT_FOUND", str(ctx.exception))
        self.assertIn("year = 2026", str(ctx.exception))

    def test_parse_query_plain_count_reads_skipprd_json(self) -> None:
        stdout = 'noise\n{"rows": [[6.0]]}\n'
        self.assertEqual(harness.parse_query_plain_count(stdout), 6)
        self.assertIsNone(harness.parse_query_plain_count("no json"))

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

    def test_assert_published_file_lake_plugins_requires_all_stems(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            plugin_dir = Path(tmp) / "plugins"
            plugin_dir.mkdir()
            (plugin_dir / "skippr-plugin-data-source-file").write_text("", encoding="utf-8")
            with self.assertRaises(harness.HarnessError):
                harness.assert_published_file_lake_plugins(str(plugin_dir))
            (plugin_dir / "skippr-plugin-data-sink-skipprlake").write_text("", encoding="utf-8")
            (plugin_dir / "skippr-plugin-schema-sink-skipprlake").write_text("", encoding="utf-8")
            harness.assert_published_file_lake_plugins(str(plugin_dir))

    def test_published_plugins_omits_local_manifest_env(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "skippr.yml"
            data = Path(tmp) / "data"
            with mock.patch.dict(
                os.environ,
                {"USE_PUBLISHED_PLUGINS": "1", "USE_LOCAL_PLUGIN_CODE": "1"},
                clear=False,
            ):
                env = harness.lake_env(config, data, None)
            self.assertNotIn("USE_LOCAL_PLUGIN_CODE", env)
            self.assertNotIn("SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR", env)
            self.assertTrue(env["SKIPPR_RUNTIME_PLUGIN_DIR"])

    def test_local_plugins_set_use_local_plugin_code(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = Path(tmp) / "skippr.yml"
            data = Path(tmp) / "data"
            env_overlay = {
                key: value
                for key, value in os.environ.items()
                if key != "USE_PUBLISHED_PLUGINS"
            }
            with mock.patch.dict(os.environ, env_overlay, clear=True):
                env = harness.lake_env(config, data, "/tmp/manifests")
            self.assertEqual(env["USE_LOCAL_PLUGIN_CODE"], "1")
            self.assertEqual(env["SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR"], "/tmp/manifests")

    def test_write_source_is_twelve_rows(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "source"
            harness.write_source(source)
            files = sorted(source.glob("event_*.json"))
            self.assertEqual(len(files), 12)
            rows = [json.loads(path.read_text(encoding="utf-8")) for path in files]
            self.assertEqual(sum(row["amount"] for row in rows), 780)
            self.assertEqual(
                [row["event_ts"] for row in rows[:6]],
                ["2026-11-07T15:04:05Z"] * 6,
            )
            self.assertEqual(
                [row["event_ts"] for row in rows[6:]],
                ["2026-11-08T09:00:00Z"] * 6,
            )


if __name__ == "__main__":
    unittest.main()
