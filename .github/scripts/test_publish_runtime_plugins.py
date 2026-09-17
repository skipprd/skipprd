import importlib.util
import urllib.parse
import sys
import unittest
from pathlib import Path


SCRIPTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS_DIR))
from runtime_plugin_targets import RuntimePluginTarget

MODULE_PATH = SCRIPTS_DIR / "publish_runtime_plugins.py"
SPEC = importlib.util.spec_from_file_location("publish_runtime_plugins", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
publish_runtime_plugins = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publish_runtime_plugins)


class PublishRuntimePluginsTests(unittest.TestCase):
    def test_public_url_prefers_public_base_url(self) -> None:
        self.assertEqual(
            publish_runtime_plugins.public_url(
                "https://install.skippr.io/releases/runtime-plugins",
                "skippr-web-install-site-prod",
                "runtime-plugins",
                "plugins",
                "s3-source",
                "versions",
                "0.1.1",
                "s3-source.json",
            ),
            "https://install.skippr.io/releases/runtime-plugins/plugins/s3-source/versions/0.1.1/s3-source.json",
        )

    def test_public_url_falls_back_to_s3(self) -> None:
        self.assertEqual(
            publish_runtime_plugins.public_url(
                "",
                "skippr-web-install-site-prod",
                "runtime-plugins",
                "latest",
                "manifest-index.json",
            ),
            "https://skippr-web-install-site-prod.s3.amazonaws.com/releases/runtime-plugins/latest/manifest-index.json",
        )

    def test_rewrite_public_url_retargets_release_path(self) -> None:
        self.assertEqual(
            publish_runtime_plugins.rewrite_public_url(
                "https://skippr-web-install-site-prod.s3.amazonaws.com/releases/runtime-plugins/plugins/athena-sink/versions/0.1.1/x86_64-unknown-linux-gnu/skippr-plugin-data-sink-athena",
                "https://install.skippr.io/releases/runtime-plugins",
                "runtime-plugins",
            ),
            "https://install.skippr.io/releases/runtime-plugins/plugins/athena-sink/versions/0.1.1/x86_64-unknown-linux-gnu/skippr-plugin-data-sink-athena",
        )

    def test_rewrite_public_url_preserves_unrelated_urls(self) -> None:
        url = "https://example.com/runtime-plugins/plugin-releases/athena-sink/0.1.1/binary"
        self.assertEqual(
            publish_runtime_plugins.rewrite_public_url(
                url,
                "https://install.skippr.io/releases/runtime-plugins",
                "runtime-plugins",
            ),
            url,
        )

    def test_published_manifest_matches_catalog(self) -> None:
        target = RuntimePluginTarget(
            triple="x86_64-unknown-linux-gnu",
            aliases=("linux-x86_64",),
            publish_artifact_dir="runtime-plugin-binaries-linux_x86",
            build_environment={"runner_baseline": "skippr-linux-x64-16"},
        )
        published = {
            "version": "0.1.1",
            "protocol_version": 10,
            "sdk_build_fingerprint": "sdk123",
            "build_checksum": "abc123",
            "artifacts": {
                "x86_64-unknown-linux-gnu": {
                    "build_environment": {"runner_baseline": "skippr-linux-x64-16"}
                }
            },
        }
        plugin = {
            "package_version": "0.1.1",
            "sdk_build_fingerprint": "sdk123",
            "checksum": "abc123",
        }
        published_index = {"sdk_build_fingerprint": "sdk123"}
        published_index_entry = {"sdk_build_fingerprint": "sdk123"}
        self.assertTrue(
            publish_runtime_plugins.published_manifest_matches_catalog(
                published,
                published_index,
                published_index_entry,
                plugin,
                [target],
                10,
            )
        )
        plugin["checksum"] = "def456"
        self.assertFalse(
            publish_runtime_plugins.published_manifest_matches_catalog(
                published,
                published_index,
                published_index_entry,
                plugin,
                [target],
                10,
            )
        )

    def test_published_manifest_requires_matching_protocol_version(self) -> None:
        target = RuntimePluginTarget(
            triple="x86_64-unknown-linux-gnu",
            aliases=("linux-x86_64",),
            publish_artifact_dir="runtime-plugin-binaries-linux_x86",
            build_environment={"runner_baseline": "skippr-linux-x64-16"},
        )
        published = {
            "version": "0.1.1",
            "protocol_version": 8,
            "sdk_build_fingerprint": "sdk123",
            "build_checksum": "abc123",
            "artifacts": {
                "x86_64-unknown-linux-gnu": {
                    "build_environment": {"runner_baseline": "skippr-linux-x64-16"}
                }
            },
        }
        plugin = {
            "package_version": "0.1.1",
            "sdk_build_fingerprint": "sdk123",
            "checksum": "abc123",
        }

        self.assertFalse(
            publish_runtime_plugins.published_manifest_matches_catalog(
                published,
                {"sdk_build_fingerprint": "sdk123"},
                {"sdk_build_fingerprint": "sdk123"},
                plugin,
                [target],
                9,
            )
        )

    def test_published_manifest_requires_matching_build_environment(self) -> None:
        target = RuntimePluginTarget(
            triple="x86_64-unknown-linux-gnu",
            aliases=("linux-x86_64",),
            publish_artifact_dir="runtime-plugin-binaries-linux_x86",
            build_environment={"runner_baseline": "skippr-linux-x64-16"},
        )
        published = {
            "version": "0.1.1",
            "protocol_version": 10,
            "sdk_build_fingerprint": "sdk123",
            "build_checksum": "abc123",
            "artifacts": {
                "x86_64-unknown-linux-gnu": {
                    "build_environment": {"runner_baseline": "skippr-linux-x64-8"}
                }
            },
        }
        plugin = {
            "package_version": "0.1.1",
            "sdk_build_fingerprint": "sdk123",
            "checksum": "abc123",
        }

        self.assertFalse(
            publish_runtime_plugins.published_manifest_matches_catalog(
                published,
                {"sdk_build_fingerprint": "sdk123"},
                {"sdk_build_fingerprint": "sdk123"},
                plugin,
                [target],
                10,
            )
        )

    def test_published_manifest_requires_matching_sdk_build_metadata(self) -> None:
        plugin = {
            "package_version": "0.1.1",
            "sdk_build_fingerprint": "current-sdk",
            "checksum": "abc123",
        }
        published = {
            "version": "0.1.1",
            "protocol_version": 10,
            "build_checksum": "abc123",
            "artifacts": {},
        }

        self.assertFalse(
            publish_runtime_plugins.published_manifest_matches_catalog(
                published,
                {},
                {},
                plugin,
                [],
                10,
            )
        )

    def test_latest_manifest_index_url_uses_latest_pointer(self) -> None:
        self.assertEqual(
            publish_runtime_plugins.latest_manifest_index_url(
                "https://install.skippr.io/releases/runtime-plugins",
                "skippr-web-install-site-prod",
                "runtime-plugins",
            ),
            "https://install.skippr.io/releases/runtime-plugins/latest/manifest-index.json",
        )

    def test_release_bundle_version_parses_semver_tags(self) -> None:
        self.assertEqual(publish_runtime_plugins.release_bundle_version("15.13.0"), "15.13.0")
        self.assertEqual(publish_runtime_plugins.release_bundle_version("v15.13.0"), "15.13.0")
        self.assertIsNone(publish_runtime_plugins.release_bundle_version("latest"))
        self.assertIsNone(publish_runtime_plugins.release_bundle_version("main"))

    def test_fresh_metadata_url_appends_cache_buster(self) -> None:
        fresh_url = publish_runtime_plugins.fresh_metadata_url(
            "https://install.skippr.io/releases/runtime-plugins/latest/manifest-index.json"
        )
        parsed = urllib.parse.urlsplit(fresh_url)
        params = dict(urllib.parse.parse_qsl(parsed.query, keep_blank_values=True))
        self.assertEqual(
            parsed.path,
            "/releases/runtime-plugins/latest/manifest-index.json",
        )
        self.assertIn("skippr_metadata_refresh", params)


if __name__ == "__main__":
    unittest.main()
