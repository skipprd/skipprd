import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPTS_DIR = Path(__file__).resolve().parent
MODULE_PATH = SCRIPTS_DIR / "bump_changed_plugin_crates.py"
SPEC = importlib.util.spec_from_file_location("bump_changed_plugin_crates", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
bump_changed_plugin_crates = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bump_changed_plugin_crates)


ATHENA_MANIFEST = """[package]
name = "skippr-plugin-data-sink-athena"
version = "0.1.12"
edition = "2021"

[dependencies]
other = "0.1.12"
"""

S3_MANIFEST = """[package]
name = "skippr-plugin-data-source-s3"
version = "0.1.7"
edition = "2021"
"""


class BumpChangedPluginCratesTests(unittest.TestCase):
    def write_workspace_file(self, workspace: Path, relative_path: str, contents: str) -> None:
        path = workspace / relative_path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")

    def write_plugin_workspace(self, workspace: Path) -> None:
        self.write_workspace_file(
            workspace, "plugins/data_sink/athena/Cargo.toml", ATHENA_MANIFEST
        )
        self.write_workspace_file(
            workspace, "plugins/data_sink/athena/src/main.rs", "fn main() {}\n"
        )
        self.write_workspace_file(
            workspace, "plugins/data_source/s3/Cargo.toml", S3_MANIFEST
        )
        self.write_workspace_file(
            workspace, "plugins/data_source/s3/src/main.rs", "fn main() {}\n"
        )
        self.write_workspace_file(
            workspace,
            "plugins/shared/api_source/src/lib.rs",
            "pub fn shared() {}\n",
        )
        self.write_workspace_file(
            workspace,
            ".host/crates/skippr-runtime-sdk/src/protocol.rs",
            "pub const RUNTIME_PROTOCOL_VERSION: u32 = 22;\n",
        )
        self.write_workspace_file(
            workspace,
            ".host/crates/skippr-runtime-sdk/Cargo.toml",
            '[package]\nname = "skippr-runtime-sdk"\nversion = "0.1.0"\n',
        )
        self.write_workspace_file(
            workspace, ".host/crates/skippr-runtime-sdk/src/lib.rs", "pub fn sdk() {}\n"
        )
        self.write_workspace_file(workspace, "Cargo.lock", "version = 4\n")

    def package_version(self, workspace: Path, relative_path: str) -> str:
        return bump_changed_plugin_crates.read_package_version(
            workspace / relative_path
        )

    def test_bump_patch_increments_last_component(self) -> None:
        self.assertEqual(bump_changed_plugin_crates.bump_patch("0.1.12"), "0.1.13")
        self.assertEqual(bump_changed_plugin_crates.bump_patch("0.1.9"), "0.1.10")

    def test_plugin_src_change_bumps_that_crate_only(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            workspace = Path(tmp_dir)
            self.write_plugin_workspace(workspace)
            bumped = bump_changed_plugin_crates.bump_changed_plugin_crates(
                workspace,
                changed_paths=["plugins/data_sink/athena/src/lib.rs"],
            )
            self.assertEqual(
                bumped,
                {"plugins/data_sink/athena": "0.1.13"},
            )
            self.assertEqual(
                self.package_version(workspace, "plugins/data_sink/athena/Cargo.toml"),
                "0.1.13",
            )
            self.assertEqual(
                self.package_version(workspace, "plugins/data_source/s3/Cargo.toml"),
                "0.1.7",
            )
            manifest = (workspace / "plugins/data_sink/athena/Cargo.toml").read_text(
                encoding="utf-8"
            )
            self.assertIn('other = "0.1.12"', manifest)

    def test_already_bumped_crate_is_left_alone(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            workspace = Path(tmp_dir)
            self.write_plugin_workspace(workspace)
            bumped = bump_changed_plugin_crates.bump_changed_plugin_crates(
                workspace,
                changed_paths=["plugins/data_sink/athena/src/lib.rs"],
                base_versions={"plugins/data_sink/athena": "0.1.11"},
            )
            self.assertEqual(bumped, {})
            self.assertEqual(
                self.package_version(workspace, "plugins/data_sink/athena/Cargo.toml"),
                "0.1.12",
            )

    def test_shared_protocol_change_bumps_every_catalog_crate(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            workspace = Path(tmp_dir)
            self.write_plugin_workspace(workspace)
            bumped = bump_changed_plugin_crates.bump_changed_plugin_crates(
                workspace,
                changed_paths=[".host/crates/skippr-runtime-sdk/src/protocol.rs"],
            )
            self.assertEqual(
                bumped,
                {
                    "plugins/data_sink/athena": "0.1.13",
                    "plugins/data_source/s3": "0.1.8",
                },
            )

    def test_sdk_and_shared_dir_change_bumps_every_catalog_crate(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            workspace = Path(tmp_dir)
            self.write_plugin_workspace(workspace)
            bumped = bump_changed_plugin_crates.bump_changed_plugin_crates(
                workspace,
                changed_paths=[
                    "plugins/shared/api_source/src/lib.rs",
                    ".host/crates/skippr-runtime-sdk/src/lib.rs",
                ],
            )
            self.assertEqual(
                bumped,
                {
                    "plugins/data_sink/athena": "0.1.13",
                    "plugins/data_source/s3": "0.1.8",
                },
            )

    def test_new_crate_without_base_version_is_not_bumped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            workspace = Path(tmp_dir)
            self.write_plugin_workspace(workspace)
            bumped = bump_changed_plugin_crates.bump_changed_plugin_crates(
                workspace,
                changed_paths=["plugins/data_sink/athena/src/lib.rs"],
                base_versions={},
            )
            self.assertEqual(bumped, {})
            self.assertEqual(
                self.package_version(workspace, "plugins/data_sink/athena/Cargo.toml"),
                "0.1.12",
            )

    def test_unrelated_paths_do_not_write_manifests(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            workspace = Path(tmp_dir)
            self.write_plugin_workspace(workspace)
            bumped = bump_changed_plugin_crates.bump_changed_plugin_crates(
                workspace,
                changed_paths=["src/lib.rs", "CHANGELOG.md", "docs/docs/install.md"],
            )
            self.assertEqual(bumped, {})
            self.assertEqual(
                self.package_version(workspace, "plugins/data_sink/athena/Cargo.toml"),
                "0.1.12",
            )
            self.assertEqual(
                self.package_version(workspace, "plugins/data_source/s3/Cargo.toml"),
                "0.1.7",
            )

    def test_git_changed_paths_include_working_tree_since_base(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            workspace = Path(tmp_dir)
            self.write_plugin_workspace(workspace)
            init_env = bump_changed_plugin_crates.git_env()
            subprocess.run(
                ["git", "init", "-b", "main"],
                cwd=workspace,
                check=True,
                capture_output=True,
                env=init_env,
            )
            git_env = bump_changed_plugin_crates.git_env(workspace)
            subprocess.run(
                ["git", "config", "user.email", "test@example.com"],
                cwd=workspace,
                check=True,
                capture_output=True,
                env=git_env,
            )
            subprocess.run(
                ["git", "config", "user.name", "test"],
                cwd=workspace,
                check=True,
                capture_output=True,
                env=git_env,
            )
            subprocess.run(
                ["git", "add", "."],
                cwd=workspace,
                check=True,
                capture_output=True,
                env=git_env,
            )
            subprocess.run(
                ["git", "-c", "core.hooksPath=/dev/null", "commit", "-m", "base"],
                cwd=workspace,
                check=True,
                capture_output=True,
                env=git_env,
            )
            base = subprocess.check_output(
                ["git", "rev-parse", "HEAD"],
                cwd=workspace,
                text=True,
                env=git_env,
            ).strip()
            self.write_workspace_file(
                workspace,
                "plugins/data_sink/athena/src/main.rs",
                "fn main() { let _ = 1; }\n",
            )
            paths = bump_changed_plugin_crates.git_changed_paths(workspace, base)
            self.assertIn("plugins/data_sink/athena/src/main.rs", paths)


if __name__ == "__main__":
    unittest.main()
