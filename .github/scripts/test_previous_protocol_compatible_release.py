import importlib.util
import os
import tempfile
import unittest
from pathlib import Path


SCRIPTS_DIR = Path(__file__).resolve().parent
MODULE_PATH = SCRIPTS_DIR / "previous_protocol_compatible_release.py"
SPEC = importlib.util.spec_from_file_location(
    "previous_protocol_compatible_release", MODULE_PATH
)
assert SPEC is not None and SPEC.loader is not None
compat = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compat)


PROTOCOL_22 = "pub const RUNTIME_PROTOCOL_VERSION: u32 = 22;\n"
PROTOCOL_21 = "pub const RUNTIME_PROTOCOL_VERSION: u32 = 21;\n"

EVOLVE_YAML = """data_sources:
  s3_bike_hire:
    S3:
      batch_size_bytes: 2048000
data_sinks:
  lake:
    SkipprLake:
      warehouse: file:///tmp
    schema_sink: schema_sinks.lake_schema
schema_sinks:
  lake_schema:
    SkipprLake:
      warehouse: file:///tmp
"""


class PreviousProtocolCompatibleReleaseTests(unittest.TestCase):
    def test_skips_when_no_older_same_protocol_tag(self) -> None:
        result = compat.previous_protocol_compatible_release(
            current_tag="18.4.0",
            current_protocol=22,
            tags=["18.4.0", "18.3.0", "0.0.0"],
            protocol_by_tag={"18.3.0": 21},
            versions_by_tag={},
            current_versions={"file": "0.1.5", "s3": "0.1.7"},
        )
        self.assertTrue(result["skip"])
        self.assertIsNone(result.get("prev_tag"))

    def test_picks_newest_older_tag_with_matching_protocol(self) -> None:
        result = compat.previous_protocol_compatible_release(
            current_tag="18.4.0",
            current_protocol=22,
            tags=["18.4.0", "18.3.5", "18.3.0", "0.0.0"],
            protocol_by_tag={"18.3.5": 22, "18.3.0": 21},
            versions_by_tag={
                "18.3.5": {
                    "file": "0.1.4",
                    "s3": "0.1.6",
                    "skipprlake": "0.2.1",
                    "skipprlake-schema": "0.2.1",
                }
            },
            current_versions={
                "file": "0.1.5",
                "s3": "0.1.7",
                "skipprlake": "0.2.2",
                "skipprlake-schema": "0.2.2",
            },
        )
        self.assertFalse(result["skip"])
        self.assertEqual(result["prev_tag"], "18.3.5")
        self.assertEqual(result["previous_versions"]["file"], "0.1.4")
        self.assertEqual(result["current_versions"]["s3"], "0.1.7")

    def test_skips_current_and_scratch_tags(self) -> None:
        result = compat.previous_protocol_compatible_release(
            current_tag="18.4.0",
            current_protocol=22,
            tags=["18.4.0", "0.0.0", "18.3.5"],
            protocol_by_tag={"18.3.5": 22, "0.0.0": 22},
            versions_by_tag={
                "18.3.5": {"file": "0.1.4", "s3": "0.1.6", "skipprlake": "0.2.1"}
            },
            current_versions={"file": "0.1.5", "s3": "0.1.7", "skipprlake": "0.2.2"},
        )
        self.assertEqual(result["prev_tag"], "18.3.5")

    def test_skips_beta_tags_as_previous_release(self) -> None:
        result = compat.previous_protocol_compatible_release(
            current_tag="18.4.0",
            current_protocol=22,
            tags=["18.4.0", "18.3.9-beta", "18.3.5", "0.0.0"],
            protocol_by_tag={
                "18.3.9-beta": 22,
                "18.3.5": 22,
            },
            versions_by_tag={
                "18.3.9-beta": {"file": "9.9.9"},
                "18.3.5": {"file": "0.1.4", "s3": "0.1.6"},
            },
            current_versions={"file": "0.1.5", "s3": "0.1.7"},
        )
        self.assertEqual(result["prev_tag"], "18.3.5")
        self.assertEqual(result["previous_versions"]["file"], "0.1.4")

    def test_skips_when_current_tag_is_beta(self) -> None:
        result = compat.previous_protocol_compatible_release(
            current_tag="18.3.9-beta",
            current_protocol=22,
            tags=["18.3.9-beta", "18.3.8", "0.0.0"],
            protocol_by_tag={"18.3.8": 22},
            versions_by_tag={"18.3.8": {"file": "0.1.7"}},
            current_versions={"file": "0.1.7"},
        )
        self.assertTrue(result["skip"])
        self.assertIsNone(result.get("prev_tag"))

    def test_parse_protocol_from_source(self) -> None:
        self.assertEqual(compat.parse_protocol_version(PROTOCOL_22), 22)
        self.assertEqual(compat.parse_protocol_version(PROTOCOL_21), 21)
        self.assertIsNone(compat.parse_protocol_version("fn main() {}\n"))

    def test_pin_runtime_plugin_versions_inserts_kind_fields(self) -> None:
        pinned = compat.pin_runtime_plugin_versions(
            EVOLVE_YAML,
            {
                "File": "0.1.4",
                "S3": "0.1.6",
                "SkipprLake": "0.2.1",
                "SkipprLakeSchema": "0.2.0",
            },
        )
        self.assertIn('    S3:\n      version: "0.1.6"\n', pinned)
        self.assertIn("data_sinks:\n  lake:\n    SkipprLake:\n      version: \"0.2.1\"\n", pinned)
        self.assertIn(
            "schema_sinks:\n  lake_schema:\n    SkipprLake:\n      version: \"0.2.0\"\n",
            pinned,
        )
        self.assertNotIn("File:", pinned)

    def test_pin_is_idempotent_when_version_already_present(self) -> None:
        once = compat.pin_runtime_plugin_versions(EVOLVE_YAML, {"S3": "0.1.6"})
        twice = compat.pin_runtime_plugin_versions(once, {"S3": "0.1.9"})
        self.assertEqual(twice.count('version: "0.1.6"'), 1)
        self.assertNotIn('version: "0.1.9"', twice)

    def test_pins_csv_round_trip(self) -> None:
        pins = {"File": "0.1.5", "S3": "0.1.7", "SkipprLake": "0.2.2"}
        encoded = compat.encode_pins(pins)
        self.assertEqual(compat.decode_pins(encoded), pins)

    def test_parse_protocol_version_ignores_host_reexport(self) -> None:
        self.assertIsNone(
            compat.parse_protocol_version("pub use skippr_runtime_sdk::protocol::*;\n")
        )
        self.assertEqual(compat.parse_protocol_version(PROTOCOL_22), 22)

    def test_protocol_at_tag_prefers_sdk_protocol_file(self) -> None:
        def fake_git(_workspace, *_args):
            spec = _args[-1]
            if spec.endswith("crates/skippr-runtime-sdk/src/protocol.rs"):
                return PROTOCOL_22
            if spec.endswith("src/runtime_plugins/protocol.rs"):
                return "pub use skippr_runtime_sdk::protocol::*;\n"
            return None

        original = compat.git_output
        compat.git_output = fake_git
        try:
            self.assertEqual(compat.protocol_at_tag(Path("/tmp"), "1.2.3"), 22)
        finally:
            compat.git_output = original

    def test_skips_when_matching_protocol_tag_has_no_plugin_versions(self) -> None:
        result = compat.previous_protocol_compatible_release(
            current_tag="18.4.0",
            current_protocol=22,
            tags=["18.4.0", "18.3.11", "0.0.0"],
            protocol_by_tag={"18.3.11": 22},
            versions_by_tag={"18.3.11": {}},
            current_versions={"file": "0.1.10"},
        )
        self.assertTrue(result["skip"])
        self.assertIsNone(result.get("prev_tag"))

    def test_current_workspace_versions_reads_skipprd_plugins_tree(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            parent = Path(tmp)
            host = parent / "skipprd"
            plugins = parent / "skipprd-plugins"
            host.mkdir()
            crate = plugins / "plugins" / "data_source" / "file"
            crate.mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "skippr-plugin-data-source-file"\nversion = "0.9.9"\n',
                encoding="utf-8",
            )
            os.environ["SKIPPR_PLUGINS_ROOT"] = str(plugins)
            try:
                versions = compat.current_workspace_versions(host)
            finally:
                os.environ.pop("SKIPPR_PLUGINS_ROOT", None)
            self.assertEqual(versions.get("file"), "0.9.9")


if __name__ == "__main__":
    unittest.main()
