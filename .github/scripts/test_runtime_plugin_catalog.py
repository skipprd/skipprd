import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS_DIR))

MODULE_PATH = SCRIPTS_DIR / "runtime_plugin_catalog.py"
SPEC = importlib.util.spec_from_file_location("runtime_plugin_catalog", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
runtime_plugin_catalog = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runtime_plugin_catalog)


class RuntimePluginCatalogTests(unittest.TestCase):
    def write_workspace_file(self, workspace: Path, relative_path: str, contents: str) -> None:
        path = workspace / relative_path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")

    def test_package_build_checksum_changes_when_shared_code_changes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            workspace = Path(tmp_dir)
            plugin_dir = workspace / "plugins" / "data_source" / "s3"
            shared_dir = workspace / "plugins" / "shared"
            self.write_workspace_file(
                workspace,
                "plugins/data_source/s3/Cargo.toml",
                "[package]\nname = 'skippr-plugin-data-source-s3'\n",
            )
            self.write_workspace_file(
                workspace,
                "plugins/data_source/s3/src/main.rs",
                "fn main() {}\n",
            )
            self.write_workspace_file(
                workspace,
                "plugins/shared/parquet_util.rs",
                "pub fn helper() {}\n",
            )

            initial = runtime_plugin_catalog.package_build_checksum(plugin_dir, workspace)

            self.write_workspace_file(
                workspace,
                "plugins/shared/parquet_util.rs",
                "pub fn helper() { println!(\"updated\"); }\n",
            )
            updated = runtime_plugin_catalog.package_build_checksum(plugin_dir, workspace)

            self.assertNotEqual(initial, updated)

    def test_package_build_checksum_ignores_unrelated_workspace_changes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            workspace = Path(tmp_dir)
            plugin_dir = workspace / "plugins" / "data_source" / "s3"
            self.write_workspace_file(
                workspace,
                "plugins/data_source/s3/Cargo.toml",
                "[package]\nname = 'skippr-plugin-data-source-s3'\n",
            )
            self.write_workspace_file(
                workspace,
                "plugins/data_source/s3/src/main.rs",
                "fn main() {}\n",
            )
            self.write_workspace_file(
                workspace,
                "plugins/shared/parquet_util.rs",
                "pub fn helper() {}\n",
            )
            self.write_workspace_file(
                workspace,
                "Cargo.lock",
                "unrelated lock contents\n",
            )

            initial = runtime_plugin_catalog.package_build_checksum(plugin_dir, workspace)

            self.write_workspace_file(
                workspace,
                "Cargo.lock",
                "changed unrelated lock contents\n",
            )
            self.write_workspace_file(
                workspace,
                "crates/skippr-runtime-sdk/src/lib.rs",
                "pub fn unrelated() {}\n",
            )
            updated = runtime_plugin_catalog.package_build_checksum(plugin_dir, workspace)

            self.assertEqual(initial, updated)

    def test_manifest_names_are_derived_from_plugin_dir_and_kind(self) -> None:
        self.assertEqual(
            runtime_plugin_catalog.manifest_filename_for_plugin(
                "plugins/data_source/http_client", "DataSource"
            ),
            "http-client-source.json",
        )
        self.assertEqual(
            runtime_plugin_catalog.manifest_name_for_plugin(
                "plugins/schema_sink/motherduck", "SchemaSink"
            ),
            "motherduck-runtime-schema",
        )

    def test_validate_plugin_metadata_requires_source_capability_for_sources(self) -> None:
        package = {"name": "skippr-plugin-data-source-s3"}
        with self.assertRaises(SystemExit):
            runtime_plugin_catalog.validate_plugin_metadata(
                package,
                {
                    "kind": "DataSource",
                    "plugin_name": "S3",
                },
            )

    def test_manifest_payload_for_catalog_entry_preserves_metadata_shape(self) -> None:
        entry = {
            "manifest_name": "s3-runtime-source",
            "manifest_kind": "DataSource",
            "plugin_name": "S3",
            "package_version": "0.1.2",
            "config_schema_version": 1,
            "args": [],
            "supports_schema": False,
            "source_capability": {
                "name": "S3",
                "guarantee_tier": "AtLeastOnce",
                "checkpoint_style": "OffsetStore",
                "bootstrap_style": "SnapshotOnly",
                "order_model": "BestEffortOrdering",
                "supports_deletes": False,
                "event_id_semantics": "BrokerAssigned",
            },
            "sink_capability": None,
        }

        manifest = runtime_plugin_catalog.manifest_payload_for_catalog_entry(
            entry,
            protocol_version=3,
            artifacts={"x86_64-unknown-linux-gnu": {"executable": "skippr-plugin-data-source-s3"}},
            executable="skippr-plugin-data-source-s3",
            build_checksum="abc123",
        )

        self.assertEqual(manifest["name"], "s3-runtime-source")
        self.assertEqual(manifest["kind"], "DataSource")
        self.assertEqual(manifest["plugin_name"], "S3")
        self.assertEqual(manifest["version"], "0.1.2")
        self.assertEqual(manifest["protocol_version"], 3)
        self.assertEqual(manifest["build_checksum"], "abc123")
        self.assertIn("source_capability", manifest)
        self.assertNotIn("sink_capability", manifest)

    def test_workspace_runtime_protocol_version_uses_current_runtime_plugin_location(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            workspace = Path(tmp_dir)
            self.write_workspace_file(
                workspace,
                "crates/skippr-runtime-sdk/src/protocol.rs",
                "pub use skippr_core::runtime_plugins::protocol::*;\n",
            )
            self.write_workspace_file(
                workspace,
                "src/runtime_plugins/protocol.rs",
                "pub const RUNTIME_PROTOCOL_VERSION: u32 = 6;\n",
            )

            self.assertEqual(
                runtime_plugin_catalog.workspace_runtime_protocol_version(workspace),
                6,
            )

    def test_workspace_runtime_protocol_version_supports_legacy_sdk_location(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            workspace = Path(tmp_dir)
            self.write_workspace_file(
                workspace,
                "crates/skippr-runtime-sdk/src/protocol.rs",
                "pub const RUNTIME_PROTOCOL_VERSION: u32 = 5;\n",
            )

            self.assertEqual(
                runtime_plugin_catalog.workspace_runtime_protocol_version(workspace),
                5,
            )


if __name__ == "__main__":
    unittest.main()
