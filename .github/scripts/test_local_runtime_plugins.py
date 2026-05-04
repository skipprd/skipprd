import importlib.util
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS_DIR))

MODULE_PATH = SCRIPTS_DIR / "local_runtime_plugins.py"
SPEC = importlib.util.spec_from_file_location("local_runtime_plugins", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
local_runtime_plugins = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = local_runtime_plugins
SPEC.loader.exec_module(local_runtime_plugins)


class LocalRuntimePluginsTests(unittest.TestCase):
    def test_configured_runtime_plugins_selects_pipeline_source_sink_and_schema(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config = Path(temp_dir) / "skippr.yml"
            config.write_text(
                """skippr:
  workspace: test
pipelines:
  p1:
    data_source: data_sources.mssql
    data_sink: data_sinks.snowflake
data_sources:
  mssql:
    Mssql:
      tables: [dbo.orders]
data_sinks:
  snowflake:
    schema_sink: schema_sinks.snowflake_schema
    Snowflake:
      database: ANALYTICS
schema_sinks:
  snowflake_schema:
    Snowflake:
      database: ANALYTICS
""",
                encoding="utf-8",
            )

            selected = local_runtime_plugins.configured_runtime_plugins(config, "p1")

        self.assertEqual(
            selected,
            {
                local_runtime_plugins.RuntimePluginRef("DataSource", "Mssql"),
                local_runtime_plugins.RuntimePluginRef("DataSink", "Snowflake"),
                local_runtime_plugins.RuntimePluginRef("SchemaSink", "Snowflake"),
            },
        )

    def test_configured_runtime_plugins_requires_pipeline_for_multi_pipeline_config(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            config = Path(temp_dir) / "skippr.yml"
            config.write_text(
                """pipelines:
  p1:
    data_source: data_sources.mssql
  p2:
    data_source: data_sources.mssql
data_sources:
  mssql:
    Mssql: {}
""",
                encoding="utf-8",
            )

            with self.assertRaises(local_runtime_plugins.LocalRuntimePluginError):
                local_runtime_plugins.configured_runtime_plugins(config, None)

    def test_builds_only_config_referenced_plugins_and_writes_manifests(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            config = root / "skippr.yml"
            output_dir = root / "manifests"
            target_dir = root / "target"
            config.write_text(
                """pipelines:
  p1:
    data_source: data_sources.mssql
    data_sink: data_sinks.snowflake
data_sources:
  mssql:
    Mssql: {}
data_sinks:
  snowflake:
    Snowflake: {}
""",
                encoding="utf-8",
            )
            fake_catalog = [
                {
                    "package_name": "skippr-plugin-data-source-mssql",
                    "package_version": "0.1.2",
                    "checksum": "mssql-checksum",
                    "binary_name": "skippr-plugin-data-source-mssql",
                    "manifest_filename": "mssql-source.json",
                    "manifest_kind": "DataSource",
                    "manifest_name": "mssql-runtime-source",
                    "plugin_name": "Mssql",
                    "supports_schema": False,
                    "config_schema_version": 1,
                    "args": [],
                    "source_capability": {"name": "mssql"},
                    "sink_capability": None,
                },
                {
                    "package_name": "skippr-plugin-data-sink-snowflake",
                    "package_version": "0.1.2",
                    "checksum": "snowflake-checksum",
                    "binary_name": "skippr-plugin-data-sink-snowflake",
                    "manifest_filename": "snowflake-sink.json",
                    "manifest_kind": "DataSink",
                    "manifest_name": "snowflake-runtime-sink",
                    "plugin_name": "Snowflake",
                    "supports_schema": True,
                    "config_schema_version": 1,
                    "args": [],
                    "source_capability": None,
                    "sink_capability": {"name": "snowflake"},
                },
                {
                    "package_name": "skippr-plugin-data-sink-athena",
                    "package_version": "0.1.2",
                    "checksum": "athena-checksum",
                    "binary_name": "skippr-plugin-data-sink-athena",
                    "manifest_filename": "athena-sink.json",
                    "manifest_kind": "DataSink",
                    "manifest_name": "athena-runtime-sink",
                    "plugin_name": "Athena",
                    "supports_schema": True,
                    "config_schema_version": 1,
                    "args": [],
                    "source_capability": None,
                    "sink_capability": {"name": "athena"},
                },
            ]

            def fake_run(command: list[str], *, env: dict[str, str] | None = None) -> None:
                self.assertIn("-p", command)
                self.assertIn("skippr-plugin-data-source-mssql", command)
                self.assertIn("skippr-plugin-data-sink-snowflake", command)
                self.assertNotIn("skippr-plugin-data-sink-athena", command)
                (target_dir / "debug").mkdir(parents=True)
                (target_dir / "debug" / "skippr-plugin-data-source-mssql").write_text(
                    "mssql", encoding="utf-8"
                )
                (target_dir / "debug" / "skippr-plugin-data-sink-snowflake").write_text(
                    "snowflake", encoding="utf-8"
                )

            with mock.patch.dict(os.environ, {"CARGO_TARGET_DIR": str(target_dir)}, clear=False):
                with mock.patch.object(
                    local_runtime_plugins,
                    "load_workspace_plugin_catalog",
                    return_value=fake_catalog,
                ), mock.patch.object(
                    local_runtime_plugins,
                    "workspace_runtime_protocol_version",
                    return_value=1,
                ), mock.patch.object(
                    local_runtime_plugins,
                    "current_rust_target_triple",
                    return_value="x86_64-unknown-linux-gnu",
                ), mock.patch.object(
                    local_runtime_plugins,
                    "run_command",
                    side_effect=fake_run,
                ):
                    local_runtime_plugins.build_local_runtime_plugins(
                        config_path=config,
                        pipeline="p1",
                        output_dir=output_dir,
                        release=False,
                    )

            self.assertTrue((output_dir / "mssql-source.json").exists())
            self.assertTrue((output_dir / "snowflake-sink.json").exists())
            self.assertFalse((output_dir / "athena-sink.json").exists())


if __name__ == "__main__":
    unittest.main()
