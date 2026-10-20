import importlib.util
import sys
import unittest
from pathlib import Path


SCRIPTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS_DIR))

MODULE_PATH = SCRIPTS_DIR / "runtime_plugin_release_plan.py"
SPEC = importlib.util.spec_from_file_location("runtime_plugin_release_plan", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
runtime_plugin_release_plan = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runtime_plugin_release_plan)


class RuntimePluginReleasePlanTests(unittest.TestCase):
    def catalog(self, sdk_build_fingerprint: str) -> list[dict]:
        return [
            {
                "package_name": "skippr-plugin-data-sink-athena",
                "package_version": "0.1.9",
                "manifest_filename": "athena-sink.json",
                "sdk_build_fingerprint": sdk_build_fingerprint,
                "checksum": "athena-checksum",
            },
            {
                "package_name": "skippr-plugin-data-source-s3",
                "package_version": "0.1.4",
                "manifest_filename": "s3-source.json",
                "sdk_build_fingerprint": sdk_build_fingerprint,
                "checksum": "s3-checksum",
            },
        ]

    def published_metadata(
        self, sdk_build_fingerprint: str
    ) -> tuple[dict, dict[str, dict]]:
        catalog = self.catalog(sdk_build_fingerprint)
        index = {
            "sdk_build_fingerprint": sdk_build_fingerprint,
            "manifests": [
                {
                    "manifest_filename": plugin["manifest_filename"],
                    "sdk_build_fingerprint": sdk_build_fingerprint,
                }
                for plugin in catalog
            ],
        }
        manifests = {
            plugin["manifest_filename"]: {
                "version": plugin["package_version"],
                "protocol_version": 17,
                "sdk_build_fingerprint": sdk_build_fingerprint,
                "build_checksum": plugin["checksum"],
                "artifacts": {},
            }
            for plugin in catalog
        }
        return index, manifests

    def test_matching_sdk_build_metadata_reuses_all_plugins(self) -> None:
        index, manifests = self.published_metadata("current-sdk")

        plan = runtime_plugin_release_plan.plan_runtime_plugin_release(
            self.catalog("current-sdk"),
            [],
            17,
            index,
            manifests,
        )

        self.assertFalse(plan["build_all"])
        self.assertEqual(plan["packages"], [])

    def test_protocol_change_same_crate_version_is_immutable(self) -> None:
        index, manifests = self.published_metadata("current-sdk")

        with self.assertRaises(SystemExit) as raised:
            runtime_plugin_release_plan.plan_runtime_plugin_release(
                self.catalog("current-sdk"),
                [],
                18,
                index,
                manifests,
            )

        message = str(raised.exception)
        self.assertIn("skippr-plugin-data-sink-athena", message)
        self.assertIn("0.1.9", message)
        self.assertIn("version paths are immutable", message)

    def test_sdk_build_change_same_crate_version_is_immutable(self) -> None:
        index, manifests = self.published_metadata("old-sdk")

        with self.assertRaises(SystemExit) as raised:
            runtime_plugin_release_plan.plan_runtime_plugin_release(
                self.catalog("current-sdk"),
                [],
                17,
                index,
                manifests,
            )

        self.assertIn("version paths are immutable", str(raised.exception))

    def test_stale_catalog_entry_same_crate_version_is_immutable(self) -> None:
        index, manifests = self.published_metadata("current-sdk")
        index["manifests"][0]["sdk_build_fingerprint"] = "stale-sdk"

        with self.assertRaises(SystemExit) as raised:
            runtime_plugin_release_plan.plan_runtime_plugin_release(
                self.catalog("current-sdk"),
                [],
                17,
                index,
                manifests,
            )

        message = str(raised.exception)
        self.assertIn("skippr-plugin-data-sink-athena", message)
        self.assertIn("version paths are immutable", message)

    def test_crate_version_bump_selects_new_path(self) -> None:
        index, manifests = self.published_metadata("current-sdk")
        catalog = self.catalog("current-sdk")
        catalog[0]["package_version"] = "0.1.10"

        plan = runtime_plugin_release_plan.plan_runtime_plugin_release(
            catalog,
            [],
            17,
            index,
            manifests,
        )

        self.assertEqual(plan["packages"], ["skippr-plugin-data-sink-athena"])
        self.assertIn(
            "version changed from 0.1.9 to 0.1.10",
            plan["package_reasons"]["skippr-plugin-data-sink-athena"],
        )


if __name__ == "__main__":
    unittest.main()
