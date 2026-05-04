import importlib.util
import os
import stat
import subprocess
import sys
import tempfile
import urllib.parse
import unittest
from pathlib import Path
from unittest import mock


SCRIPTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS_DIR))

MODULE_PATH = SCRIPTS_DIR / "runtime_e2e_harness.py"
SPEC = importlib.util.spec_from_file_location("runtime_e2e_harness", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
runtime_e2e_harness = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = runtime_e2e_harness
SPEC.loader.exec_module(runtime_e2e_harness)


class RuntimeE2eHarnessTests(unittest.TestCase):
    def test_scenarios_cover_expected_names(self) -> None:
        self.assertEqual(
            sorted(runtime_e2e_harness.SCENARIOS.keys()),
            [
                "bike_hire",
                "bike_hire_many",
                "bike_hire_s3_wal_many",
                "deadletters_test",
                "dynamodb_iceberg_types_cdc",
                "mssql_iceberg_debug_linux",
                "mssql_iceberg_debug_windows",
                "mysql_iceberg_types_cdc",
                "postgres_iceberg_types_cdc",
            ],
        )

    def test_resolve_skipprd_accepts_binary_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            binary = Path(temp_dir) / "skipprd"
            binary.write_text("#!/bin/sh\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)

            resolved = runtime_e2e_harness.resolve_skipprd(str(binary))

            self.assertEqual(resolved, binary.resolve())

    def test_resolve_skipprd_accepts_artifact_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            artifact_dir = Path(temp_dir) / "skipprd-linux_x86"
            artifact_dir.mkdir()
            binary = artifact_dir / "skipprd"
            binary.write_text("#!/bin/sh\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)

            resolved = runtime_e2e_harness.resolve_skipprd(str(artifact_dir))

            self.assertEqual(resolved, binary.resolve())

    def test_skippr_engine_command_prefix_passes_config_to_installed_cli(self) -> None:
        command = runtime_e2e_harness.skippr_engine_command_prefix(
            Path("/usr/local/bin/skippr"),
            {"SKIPPR_CONFIG_FILE": "/tmp/skippr-runtime-e2e/skippr.yml"},
        )

        self.assertEqual(
            command,
            [
                "/usr/local/bin/skippr",
                "--config",
                "/tmp/skippr-runtime-e2e/skippr.yml",
            ],
        )

    def test_skippr_engine_command_prefix_leaves_skipprd_artifact_unchanged(self) -> None:
        command = runtime_e2e_harness.skippr_engine_command_prefix(
            Path("/tmp/skipprd-linux_x86/skipprd"),
            {"SKIPPR_CONFIG_FILE": "/tmp/skippr-runtime-e2e/skippr.yml"},
        )

        self.assertEqual(command, ["/tmp/skipprd-linux_x86/skipprd"])

    def test_find_downloaded_plugins_matches_expected_patterns(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            plugin_root = Path(temp_dir)
            matching = plugin_root / "nested" / "skippr-plugin-data-sink-athena"
            matching.parent.mkdir(parents=True)
            matching.write_text("", encoding="utf-8")
            non_matching = plugin_root / "skipprd"
            non_matching.write_text("", encoding="utf-8")

            found = runtime_e2e_harness.find_downloaded_plugins(plugin_root)

            self.assertEqual(found, [matching])

    def test_assert_no_bundled_runtime_plugins_rejects_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            artifact_dir = Path(temp_dir)
            binary = artifact_dir / "skipprd"
            binary.write_text("#!/bin/sh\n", encoding="utf-8")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            (artifact_dir / "skippr-plugin-data-sink-athena").write_text(
                "",
                encoding="utf-8",
            )

            with self.assertRaises(runtime_e2e_harness.HarnessError):
                runtime_e2e_harness.assert_no_bundled_runtime_plugins(binary)

    def test_runtime_release_manifest_filenames_include_all_sources_and_smoke_support(self) -> None:
        catalog = [
            {"manifest_filename": "file-source.json", "manifest_kind": "DataSource"},
            {"manifest_filename": "postgres-source.json", "manifest_kind": "DataSource"},
            {"manifest_filename": "file-sink.json", "manifest_kind": "DataSink"},
            {"manifest_filename": "postgres-sink.json", "manifest_kind": "DataSink"},
            {"manifest_filename": "postgres-schema.json", "manifest_kind": "SchemaSink"},
        ]

        manifest_filenames = runtime_e2e_harness.runtime_release_manifest_filenames(catalog)

        self.assertEqual(
            manifest_filenames,
            [
                "file-sink.json",
                "file-source.json",
                "postgres-schema.json",
                "postgres-sink.json",
                "postgres-source.json",
            ],
        )

    def test_runtime_release_manifest_filenames_require_smoke_support_manifests(self) -> None:
        catalog = [
            {"manifest_filename": "file-source.json", "manifest_kind": "DataSource"},
            {"manifest_filename": "postgres-source.json", "manifest_kind": "DataSource"},
            {"manifest_filename": "postgres-sink.json", "manifest_kind": "DataSink"},
            {"manifest_filename": "postgres-schema.json", "manifest_kind": "SchemaSink"},
        ]

        with self.assertRaises(runtime_e2e_harness.HarnessError):
            runtime_e2e_harness.runtime_release_manifest_filenames(catalog)

    def test_scenario_pipeline_name_requires_single_pipeline(self) -> None:
        scenario = runtime_e2e_harness.Scenario(
            name="mixed",
            config_path=Path("/tmp/mixed.yml"),
            smoke_runs=(runtime_e2e_harness.SyncRun(pipeline="one"),),
            full_runs=(runtime_e2e_harness.SyncRun(pipeline="two"),),
        )

        with self.assertRaises(runtime_e2e_harness.HarnessError):
            runtime_e2e_harness.scenario_pipeline_name(scenario)

    def test_published_target_for_architecture_name_resolves_linux_release_bundle(self) -> None:
        target = runtime_e2e_harness.published_target_for_architecture_name("linux_x86")

        self.assertEqual(target.triple, "x86_64-unknown-linux-gnu")
        self.assertEqual(target.publish_artifact_dir, "runtime-plugin-binaries-linux_x86")

    def test_runtime_plugin_version_config_text_injects_bike_hire_versions(self) -> None:
        config_text = """data_sources:
  s3_bike_hire:
    S3:
      s3_bucket: skippr-e2e-sample-data
data_sinks:
  test_datalake:
    Athena:
      s3_bucket: skippr-e2e-sample-data-output
schema_sinks:
  glue_bikehire:
    Glue:
      glue_database_name: bikehire
"""

        rewritten = runtime_e2e_harness.runtime_plugin_version_config_text(
            "bike_hire",
            config_text,
            {
                "S3": "8.1.0",
                "Athena": "8.2.0",
                "Glue": "8.3.0",
            },
        )

        self.assertIn('S3:\n      version: "8.1.0"\n', rewritten)
        self.assertIn('Athena:\n      version: "8.2.0"\n', rewritten)
        self.assertIn('Glue:\n      version: "8.3.0"\n', rewritten)

    def test_runtime_plugin_version_config_text_ignores_missing_values(self) -> None:
        config_text = """data_sources:
  s3_bike_hire:
    S3:
      s3_bucket: skippr-e2e-sample-data
data_sinks:
  test_datalake:
    Athena:
      s3_bucket: skippr-e2e-sample-data-output
schema_sinks:
  glue_bikehire:
    Glue:
      glue_database_name: bikehire
"""

        rewritten = runtime_e2e_harness.runtime_plugin_version_config_text(
            "bike_hire", config_text, {"Athena": "8.1.0"}
        )

        self.assertEqual(rewritten.count('version: "8.1.0"'), 1)

    def test_parse_runtime_plugin_versions_requires_plugin_equals_version(self) -> None:
        with self.assertRaises(runtime_e2e_harness.HarnessError):
            runtime_e2e_harness.parse_runtime_plugin_versions(["Athena"])

    def test_local_runtime_config_text_rejects_explicit_manifests(self) -> None:
        config_text = """pipelines:
  bike_hire:
    data_source: data_sources.s3_bike_hire
    data_sink: data_sinks.test_datalake
"""
        local_manifests = runtime_e2e_harness.LocalStagedRuntimeManifests(
            manifest_dir=Path("/tmp/local-runtime"),
            manifest_paths={
                "runtime_s3_source": Path("/tmp/local-runtime/s3-source.json"),
                "runtime_athena_sink": Path("/tmp/local-runtime/athena-sink.json"),
                "runtime_glue_schema": Path("/tmp/local-runtime/glue-schema.json"),
            },
            manifest_versions={
                "runtime_s3_source": "0.1.1",
                "runtime_athena_sink": "0.1.1",
                "runtime_glue_schema": "0.1.1",
            },
        )

        with self.assertRaises(runtime_e2e_harness.HarnessError):
            runtime_e2e_harness.local_runtime_config_text(
                "bike_hire",
                config_text,
                local_manifests,
            )

    def test_load_local_runtime_manifests_requires_expected_files(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            manifest_dir = Path(temp_dir)
            s3_manifest = (
                manifest_dir / "plugins" / "s3-source" / "versions" / "0.1.1" / "s3-source.json"
            )
            athena_manifest = (
                manifest_dir
                / "plugins"
                / "athena-sink"
                / "versions"
                / "0.2.0"
                / "athena-sink.json"
            )
            glue_manifest = (
                manifest_dir
                / "plugins"
                / "glue-schema"
                / "versions"
                / "0.3.0"
                / "glue-schema.json"
            )
            s3_manifest.parent.mkdir(parents=True, exist_ok=True)
            athena_manifest.parent.mkdir(parents=True, exist_ok=True)
            glue_manifest.parent.mkdir(parents=True, exist_ok=True)
            s3_manifest.write_text(
                '{"version":"0.1.1"}\n',
                encoding="utf-8",
            )
            athena_manifest.write_text(
                '{"version":"0.2.0"}\n',
                encoding="utf-8",
            )
            glue_manifest.write_text(
                '{"version":"0.3.0"}\n',
                encoding="utf-8",
            )

            with mock.patch.object(
                runtime_e2e_harness,
                "load_workspace_plugin_catalog",
                return_value=[
                    {
                        "manifest_filename": "s3-source.json",
                        "manifest_stem": "s3-source",
                        "package_version": "0.1.1",
                    },
                    {
                        "manifest_filename": "athena-sink.json",
                        "manifest_stem": "athena-sink",
                        "package_version": "0.2.0",
                    },
                    {
                        "manifest_filename": "glue-schema.json",
                        "manifest_stem": "glue-schema",
                        "package_version": "0.3.0",
                    },
                    {
                        "manifest_filename": "postgres-source.json",
                        "manifest_stem": "postgres-source",
                        "package_version": "0.4.0",
                    },
                    {
                        "manifest_filename": "mssql-source.json",
                        "manifest_stem": "mssql-source",
                        "package_version": "0.5.0",
                    },
                    {
                        "manifest_filename": "mysql-source.json",
                        "manifest_stem": "mysql-source",
                        "package_version": "0.6.0",
                    },
                    {
                        "manifest_filename": "dynamodb-source.json",
                        "manifest_stem": "dynamodb-source",
                        "package_version": "0.7.0",
                    },
                    {
                        "manifest_filename": "iceberg-sink.json",
                        "manifest_stem": "iceberg-sink",
                        "package_version": "0.8.0",
                    },
                    {
                        "manifest_filename": "iceberg-schema.json",
                        "manifest_stem": "iceberg-schema",
                        "package_version": "0.9.0",
                    },
                    {
                        "manifest_filename": "snowflake-sink.json",
                        "manifest_stem": "snowflake-sink",
                        "package_version": "0.10.0",
                    },
                    {
                        "manifest_filename": "snowflake-schema.json",
                        "manifest_stem": "snowflake-schema",
                        "package_version": "0.11.0",
                    },
                ],
            ):
                loaded = runtime_e2e_harness.load_local_runtime_manifests(manifest_dir)

            self.assertEqual(
                loaded.manifest_versions,
                {
                    "runtime_s3_source": "0.1.1",
                    "runtime_athena_sink": "0.2.0",
                    "runtime_glue_schema": "0.3.0",
                },
            )
            self.assertEqual(
                sorted(loaded.manifest_paths.keys()),
                [
                    "runtime_athena_sink",
                    "runtime_glue_schema",
                    "runtime_s3_source",
                ],
            )

    def test_verify_runtime_release_artifacts_rejects_manifest_version_drift(self) -> None:
        target = runtime_e2e_harness.published_target_for_architecture_name("linux_x86")
        manifests = {
            "athena-sink.json": runtime_e2e_harness.DownloadedRuntimeManifest(
                path=Path("/tmp/athena-sink.json"),
                payload={
                    "version": "0.1.0",
                    "protocol_version": 2,
                    "artifacts": {},
                },
            )
        }

        with self.assertRaises(runtime_e2e_harness.HarnessError):
            runtime_e2e_harness.verify_runtime_release_artifacts(
                manifests,
                source_manifests=set(),
                full_download_manifests=set(),
                catalog_by_manifest={
                    "athena-sink.json": {
                        "package_version": "0.1.1",
                        "checksum": "checksum",
                    }
                },
                expected_versions_by_manifest=None,
                expected_protocol_version=2,
                target=target,
                download_dir=Path("/tmp"),
            )

    def test_verify_runtime_release_artifacts_ignores_build_checksum_mismatch(self) -> None:
        target = runtime_e2e_harness.published_target_for_architecture_name("linux_x86")
        manifests = {
            "athena-sink.json": runtime_e2e_harness.DownloadedRuntimeManifest(
                path=Path("/tmp/athena-sink.json"),
                payload={
                    "version": "0.1.1",
                    "protocol_version": 2,
                    "build_checksum": "stale-checksum",
                    "artifacts": {},
                },
            )
        }

        runtime_e2e_harness.verify_runtime_release_artifacts(
            manifests,
            source_manifests=set(),
            full_download_manifests=set(),
            catalog_by_manifest={
                "athena-sink.json": {
                    "package_version": "0.1.1",
                    "checksum": "fresh-checksum",
                }
            },
            expected_versions_by_manifest=None,
            expected_protocol_version=2,
            target=target,
            download_dir=Path("/tmp"),
        )

    def test_public_release_base_url_prefers_install_site(self) -> None:
        self.assertEqual(
            runtime_e2e_harness.public_release_base_url(
                "skippr-web-install-site-prod",
                "runtime-plugins",
            ),
            "https://install.skippr.io/releases/runtime-plugins",
        )

    def test_fresh_metadata_url_appends_cache_buster(self) -> None:
        fresh_url = runtime_e2e_harness.fresh_metadata_url(
            "https://install.skippr.io/releases/runtime-plugins/latest/manifest-index.json"
        )
        parsed = urllib.parse.urlsplit(fresh_url)
        params = dict(urllib.parse.parse_qsl(parsed.query, keep_blank_values=True))
        self.assertEqual(
            parsed.path,
            "/releases/runtime-plugins/latest/manifest-index.json",
        )
        self.assertIn("skippr_metadata_refresh", params)

    def test_purge_dynamodb_skips_missing_table(self) -> None:
        missing_table = subprocess.CompletedProcess(
            args=["aws", "dynamodb", "scan"],
            returncode=254,
            stdout="",
            stderr=(
                "An error occurred (ResourceNotFoundException) when calling the Scan "
                "operation: Requested resource not found"
            ),
        )

        with (
            mock.patch.object(
                runtime_e2e_harness, "ensure_tool", return_value="/usr/bin/aws"
            ),
            mock.patch.object(
                runtime_e2e_harness.subprocess, "run", return_value=missing_table
            ) as mock_run,
        ):
            runtime_e2e_harness.purge_dynamodb({})

        mock_run.assert_called_once()

    def test_ensure_soda_installed_uses_virtualenv(self) -> None:
        original_installed = runtime_e2e_harness.SODA_INSTALLED
        original_venv_dir = runtime_e2e_harness.SODA_VENV_DIR
        commands: list[list[str]] = []

        with tempfile.TemporaryDirectory() as temp_dir:
            venv_dir = Path(temp_dir)
            bin_dir = venv_dir / ("Scripts" if os.name == "nt" else "bin")
            python_name = "python.exe" if os.name == "nt" else "python"
            soda_name = "soda.exe" if os.name == "nt" else "soda"

            def fake_run(command, **_kwargs):
                commands.append(command)
                bin_dir.mkdir(parents=True, exist_ok=True)
                (bin_dir / python_name).write_text("", encoding="utf-8")
                (bin_dir / soda_name).write_text("", encoding="utf-8")
                return subprocess.CompletedProcess(args=command, returncode=0)

            try:
                runtime_e2e_harness.SODA_INSTALLED = False
                runtime_e2e_harness.SODA_VENV_DIR = None
                with (
                    mock.patch.object(
                        runtime_e2e_harness,
                        "resolve_soda_python",
                        return_value="/usr/local/bin/python3.11",
                    ),
                    mock.patch.object(
                        runtime_e2e_harness.tempfile, "mkdtemp", return_value=temp_dir
                    ),
                    mock.patch.object(
                        runtime_e2e_harness, "run_command", side_effect=fake_run
                    ),
                ):
                    runtime_e2e_harness.ensure_soda_installed()

                expected_python = str(bin_dir / python_name)
                expected_soda = str(bin_dir / soda_name)
                self.assertEqual(
                    commands,
                    [
                        ["/usr/local/bin/python3.11", "-m", "venv", temp_dir],
                        [
                            expected_python,
                            "-m",
                            "pip",
                            "install",
                            "setuptools",
                            "soda-core-athena",
                        ],
                    ],
                )
                self.assertEqual(runtime_e2e_harness.soda_executable(), expected_soda)
            finally:
                runtime_e2e_harness.SODA_INSTALLED = original_installed
                runtime_e2e_harness.SODA_VENV_DIR = original_venv_dir

    def test_resolve_soda_python_prefers_distutils_compatible_interpreter(self) -> None:
        def fake_which(name: str) -> str | None:
            mapping = {
                "python3.11": "/usr/local/bin/python3.11",
                "python3.10": None,
                "python3": "/opt/homebrew/bin/python3",
            }
            return mapping.get(name)

        def fake_run(command, **_kwargs):
            path = command[0]
            if path == "/usr/local/bin/python3.11":
                return subprocess.CompletedProcess(args=command, returncode=0)
            return subprocess.CompletedProcess(args=command, returncode=1)

        with (
            mock.patch.object(runtime_e2e_harness, "sys") as mock_sys,
            mock.patch.object(runtime_e2e_harness.shutil, "which", side_effect=fake_which),
            mock.patch.object(runtime_e2e_harness.subprocess, "run", side_effect=fake_run),
        ):
            mock_sys.executable = "/opt/homebrew/bin/python3"
            resolved = runtime_e2e_harness.resolve_soda_python()

        self.assertEqual(resolved, "/usr/local/bin/python3.11")

    def test_run_command_allows_signal_exit_when_shell_code_is_whitelisted(self) -> None:
        killed = subprocess.CompletedProcess(
            args=["skipprd", "sync"],
            returncode=-9,
            stdout="",
            stderr="",
        )

        with mock.patch.object(
            runtime_e2e_harness.subprocess, "run", return_value=killed
        ):
            completed = runtime_e2e_harness.run_command(
                ["skipprd", "sync"], allow_exit_codes=(137,)
            )

        self.assertEqual(completed.returncode, -9)

    def test_local_plugin_env_skips_download_assertion(self) -> None:
        scenario = runtime_e2e_harness.Scenario(
            name="local_plugins",
            config_path=Path("/tmp/local_plugins.yml"),
            smoke_runs=(),
            full_runs=(),
        )
        skipprd = Path("/tmp/skipprd")
        with (
            mock.patch.object(
                runtime_e2e_harness,
                "prepare_local_state",
            ),
            mock.patch.object(
                runtime_e2e_harness,
                "prepare_aws_state",
            ),
            mock.patch.object(
                runtime_e2e_harness,
                "materialize_scenario_config",
                return_value=Path("/tmp/local_plugins.yml"),
            ),
            mock.patch.object(
                runtime_e2e_harness,
                "base_environment",
                return_value={
                    "USE_LOCAL_PLUGIN_CODE": "1",
                    "SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR": "/tmp/local-manifests",
                },
            ),
            mock.patch.object(
                runtime_e2e_harness,
                "assert_runtime_plugins_downloaded",
            ) as assert_downloaded,
        ):
            runtime_e2e_harness.run_scenario(
                scenario,
                mode="smoke",
                skipprd=skipprd,
                prepare=False,
                skip_dynamodb=True,
                runtime_plugin_versions=None,
                local_runtime_manifests=None,
                namespace=None,
            )

        assert_downloaded.assert_not_called()


if __name__ == "__main__":
    unittest.main()
