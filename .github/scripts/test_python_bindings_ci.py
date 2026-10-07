#!/usr/bin/env python3
"""CI must build the skippr wheel and run Session tests."""

from __future__ import annotations

import importlib.util
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CI = ROOT / ".github" / "workflows" / "ci.yml"
TEST_PYTHON = ROOT / "scripts" / "test-python.sh"
DARWIN_RUSTC_WRAPPER = ROOT / "scripts" / "darwin-rustc-wrapper.py"
SET_VERSION = ROOT / ".github" / "scripts" / "set_root_package_version.py"
PYTHON_CARGO = ROOT / "python" / "Cargo.toml"
WHEEL_VERSION = ROOT / ".github" / "scripts" / "python_wheel_version.py"


def load_set_version():
    spec = importlib.util.spec_from_file_location("set_root_package_version", SET_VERSION)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def load_wheel_version():
    spec = importlib.util.spec_from_file_location("python_wheel_version", WHEEL_VERSION)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class PythonBindingsCiTests(unittest.TestCase):
    def test_ci_runs_python_binding_script(self):
        text = CI.read_text(encoding="utf-8")
        self.assertTrue(text.startswith("name: Python CI/CD Pipeline\n"))
        self.assertEqual(CI.name, "ci.yml")
        self.assertIn("scripts/test-python.sh", text)
        self.assertIn("test_python_bindings_ci.py", text)
        self.assertIn("test_rust_ci.py", text)
        self.assertIn("test_publish_runtime_plugins.py", text)
        self.assertIn("cargo test -p skipprd --lib", text)
        self.assertIn("skippr-connect-gen", text)
        self.assertNotIn("cargo test --workspace", text)

    def test_write_document_uses_posix_fsync_not_apple_fullfsync(self):
        text = (ROOT / "src" / "connect.rs").read_text(encoding="utf-8")
        helper = text.split("fn durable_config_sync", 1)[1].split("pub fn write_document", 1)[0]
        unix = helper.split("#[cfg(unix)]", 1)[1].split("#[cfg(not(unix))]", 1)[0]
        self.assertIn("libc::fsync", unix)
        self.assertNotIn("accept_config_barrier", helper)
        self.assertNotIn("libc::EIO", helper)
        self.assertNotIn("sync_all", unix)
        self.assertNotIn("sync_data", unix)
        self.assertNotIn("F_FULLFSYNC", unix)
        write = text.split("pub fn write_document", 1)[1].split(
            "pub fn yaml_scalar_from_string", 1
        )[0]
        self.assertIn("durable_config_sync(&file)", write)
        self.assertIn("fs::rename", write)
        self.assertIn("helpers::fsync::fsync_dir(parent)", write)
        self.assertNotIn("File::open(parent)", write)
        self.assertNotIn("file.sync_all()", write)
        self.assertNotIn("file.sync_data()", write)
        fsync_dir = (ROOT / "src" / "helpers" / "fsync.rs").read_text(encoding="utf-8")
        not_windows, windows = fsync_dir.split("#[cfg(windows)]", 1)
        self.assertIn("#[cfg(not(windows))]", not_windows)
        self.assertIn("File::open(dir)?.sync_all()", not_windows.split("#[cfg(not(windows))]", 1)[1])
        self.assertNotIn("File::open", windows)
        self.assertIn("Ok(())", windows)

    def test_python_script_builds_wheel_and_runs_session_tests(self):
        script = TEST_PYTHON.read_text(encoding="utf-8")
        self.assertIn("maturin develop", script)
        self.assertIn("python/tests", script)
        self.assertIn("maturin build --release", script)
        self.assertIn("PYPI_WHEEL_MAX_BYTES", script)
        self.assertIn("100 * 1024 * 1024", script)
        self.assertIn("Darwin", script)
        self.assertIn("CARGO_TARGET_DIR", script)
        self.assertIn('"$ROOT/target/maturin"', script)
        self.assertNotIn("/var/tmp/cargo-target", script)
        self.assertIn("debug/build", script)
        self.assertIn("RUSTC_WRAPPER", script)
        self.assertIn("darwin-rustc-wrapper.py", script)
        self.assertNotIn("config.toml", script)
        self.assertNotIn("[build]", script)
        self.assertIn("CARGO_ENCODED_RUSTFLAGS", script)
        self.assertIn("scripts/bin/cargo", script)
        self.assertIn("export CARGO=", script)
        self.assertNotIn("/usr/local/cargo/bin/cargo", script)

    def test_release_profile_strips_debug_so_pypi_fits(self):
        text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        release = text.split("[profile.release]", 1)[1].split("[profile.", 1)[0]
        self.assertIn("debug = false", release)
        self.assertIn('strip = "symbols"', release)
        profiling = text.split("[profile.profiling]", 1)[1]
        self.assertIn("debug = true", profiling)
        self.assertIn('strip = "none"', profiling)
        docs = (ROOT / "docs" / "docs" / "maintainers" / "release-workflow.md").read_text(
            encoding="utf-8"
        )
        self.assertIn("100 MB", docs)
        self.assertIn("strip", docs)

    def test_ci_builds_on_github_linux_x86(self):
        text = CI.read_text(encoding="utf-8")
        self.assertRegex(text, r"(?m)^    runs-on: ubuntu-latest$")
        self.assertNotRegex(text, r"(?m)^    runs-on: \[self-hosted")
        self.assertNotRegex(text, r"(?m)^  darwin:")
        self.assertRegex(text, r"(?m)^  # darwin:")
        self.assertNotIn("macos-latest", text)
        self.assertNotIn("windows-latest", text)
        self.assertNotIn("depot", text)
        linux = text.split("\n  linux:", 1)[1].split("\n  python-publish:", 1)[0]
        self.assertIn("runs-on: ubuntu-latest", linux)
        self.assertNotIn("skippr-linux-x64-16", linux)
        self.assertIn("working-directory: skipprd", text)
        self.assertIn("path: skipprd", text)
        self.assertIn("skipprd/target/wheels/*.whl", text)
        self.assertIn("cargo test -p skipprd --lib", text)

    def test_ci_publishes_wheels_with_pypi_oidc(self):
        text = CI.read_text(encoding="utf-8")
        self.assertEqual(CI.name, "ci.yml")
        self.assertIn("python-publish:", text)
        self.assertIn("pypa/gh-action-pypi-publish", text)
        self.assertIn("id-token: write", text)
        self.assertIn("upload-artifact", text)
        self.assertNotIn("set_root_package_version.py", text)
        self.assertNotIn("PYPI_API_TOKEN", text)
        self.assertNotIn("TWINE_PASSWORD", text)
        self.assertIn("tags:", text)
        self.assertIn('- "[0-9]*"', text)
        self.assertIn('- "python-v*"', text)
        self.assertNotIn('- "v*"', text)
        cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
        self.assertIn("publish = false", cargo.split("[workspace]", 1)[0])
        agents = (ROOT / "AGENTS.md").read_text(encoding="utf-8")
        self.assertIn("`0.0.0`", agents)
        self.assertNotIn("`v0.0.0`", agents)
        publish = text.split("python-publish:", 1)[1].split("\n  # Disabled for now:", 1)[0]
        self.assertNotIn("environment:", publish)
        self.assertIn("attestations: false", publish)
        self.assertIn("runs-on: ubuntu-latest", publish)
        self.assertIn("needs: [linux]", publish)
        self.assertNotIn("darwin", publish)
        self.assertNotIn("skippr-linux-x64-16", publish)
        self.assertNotIn("refs/heads/main", publish)
        self.assertNotIn("refs/heads/master", publish)
        self.assertIn("startsWith(github.ref, 'refs/tags/')", publish)
        self.assertIn("github.ref != 'refs/tags/0.0.0'", publish)
        self.assertIn("!startsWith(github.ref, 'refs/tags/test')", publish)
        self.assertIn("!startsWith(github.ref, 'refs/tags/python-')", publish)
        self.assertNotIn("startsWith(github.ref, 'refs/tags/python-v')", publish)
        self.assertIn("python_wheel_version.py", publish)
        self.assertNotIn("github.ref != 'refs/tags/v0.0.0'", publish)
        self.assertNotRegex(publish, r"startsWith\(github\.ref, 'refs/tags/v'\)")
        self.assertIn("skipprd/cloud", text)
        self.assertIn("path: cloud", text)
        self.assertIn("path: skipprd", text)
        self.assertIn("SKIPPR_CLOUD_CHECKOUT_TOKEN", text)

    def test_python_ci_runs_only_on_tags(self):
        text = CI.read_text(encoding="utf-8")
        header = text.split("\njobs:", 1)[0]
        self.assertIn("tags:", header)
        self.assertIn('- "[0-9]*"', header)
        self.assertIn('- "python-v*"', header)
        self.assertNotIn("branches:", header)
        self.assertNotIn("main", header)
        self.assertNotIn("master", header)
        self.assertNotIn("pull_request:", header)
        self.assertIn("workflow_dispatch:", header)

    def test_release_workflow_registers_pypi_project_skippr(self):
        text = (ROOT / "docs" / "docs" / "maintainers" / "release-workflow.md").read_text(
            encoding="utf-8"
        )
        self.assertIn("- Project: `skippr`", text)
        self.assertIn("- Workflow name: `ci.yml`", text)
        self.assertNotIn("- Project: `skipprd`", text)
        self.assertNotIn("not engine unprefixed host tags", text)
        self.assertIn("same unprefixed engine tags", text)
        self.assertIn("`python-v*`", text)
        self.assertNotIn("runs on `main` or `python-v*`", text)
        self.assertIn("`0.0.0`", text)
        self.assertIn("from the tag name (`1.2.3`)", text)
        self.assertNotIn("optionally with a `v` prefix", text)
        self.assertNotIn("stamps the root host package version from the tag name (`v1.2.3`", text)

    def test_install_docs_pin_unprefixed_engine_version(self):
        install = (ROOT / "docs" / "docs" / "getting-started" / "install.md").read_text(
            encoding="utf-8"
        )
        self.assertIn("SKIPPR_VERSION=6.10.0", install)
        self.assertNotIn("SKIPPR_VERSION=v", install)
        script = (ROOT / "install.sh").read_text(encoding="utf-8")
        self.assertIn("unprefixed semver", script)
        self.assertIn(r"^[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*$", script)

    def test_host_release_action_stamps_unprefixed_tags_only(self):
        action = (
            ROOT / ".github" / "actions" / "rust-build-release" / "action.yaml"
        ).read_text(encoding="utf-8")
        self.assertIn("set_root_package_version.py", action)
        self.assertIn("^[0-9]+\\.[0-9]+\\.[0-9]+$", action)
        self.assertIn("unprefixed semver", action)
        self.assertIn('echo "SKIPPR_CLI_VERSION=$version" >> "$GITHUB_ENV"', action)
        self.assertNotIn('export SKIPPR_CLI_VERSION="${GITHUB_REF_NAME}"', action)

    def test_pyproject_declares_license_and_readme(self):
        text = (ROOT / "pyproject.toml").read_text(encoding="utf-8")
        self.assertRegex(text, r'(?m)^name\s*=\s*"skippr"$')
        self.assertRegex(text, r'(?m)^readme\s*=')
        self.assertIn("LICENSE", text)
        self.assertNotIn("[project.scripts]", text)

    def test_python_module_name_is_skippr(self):
        cargo = PYTHON_CARGO.read_text(encoding="utf-8")
        self.assertIn('name = "skippr"\ncrate-type = ["cdylib"]', cargo)
        pyproject = (ROOT / "pyproject.toml").read_text(encoding="utf-8")
        self.assertIn('module-name = "skippr"', pyproject)
        self.assertEqual(load_wheel_version().PYPI_PROJECT, "skippr")

    def test_python_wheel_is_abi3_py310(self):
        text = PYTHON_CARGO.read_text(encoding="utf-8")
        self.assertIn("abi3-py310", text)

    def test_precommit_hook_runs_lib_tests(self):
        hook = (ROOT / ".githooks" / "pre-commit").read_text(encoding="utf-8")
        script = (ROOT / "scripts" / "precommit.sh").read_text(encoding="utf-8")
        installer = (ROOT / "scripts" / "install-git-hooks.sh").read_text(encoding="utf-8")
        self.assertIn("scripts/precommit.sh", hook)
        self.assertIn("cargo test -p skipprd --lib", script)
        self.assertIn("skippr-connect-gen", script)
        self.assertIn("test_python_bindings_ci.py", script)
        self.assertIn("test_rust_ci.py", script)
        self.assertIn("test_publish_runtime_plugins.py", script)
        self.assertIn(".githooks/pre-commit", installer)

    def test_python_semver_is_independent_of_engine_tags(self):
        module = load_wheel_version()
        self.assertEqual(module.python_semver(), "17.0.0")
        self.assertTrue(module.should_publish("17.0.0", published=False))
        self.assertFalse(module.should_publish("17.0.0", published=True))
        self.assertFalse(module.should_publish("0.0.0", published=False))

    def test_set_root_package_version_does_not_stamp_python(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "python").mkdir()
            shutil.copy(ROOT / "Cargo.toml", root / "Cargo.toml")
            shutil.copy(ROOT / "python" / "Cargo.toml", root / "python" / "Cargo.toml")
            shutil.copy(ROOT / "pyproject.toml", root / "pyproject.toml")
            shutil.copy(ROOT / "Cargo.lock", root / "Cargo.lock")
            subprocess.check_call(
                [
                    "python3",
                    str(SET_VERSION),
                    "--workspace",
                    str(root),
                    "--version",
                    "9.8.7",
                ]
            )
            pyproject = (root / "pyproject.toml").read_text(encoding="utf-8")
            python_cargo = (root / "python" / "Cargo.toml").read_text(encoding="utf-8")
            lock = (root / "Cargo.lock").read_text(encoding="utf-8")
            self.assertRegex((root / "Cargo.toml").read_text(encoding="utf-8"), r'(?m)^version = "9\.8\.7"$')
            self.assertRegex(pyproject, r'(?m)^version = "17\.0\.0"$')
            self.assertRegex(python_cargo, r'(?m)^version = "17\.0\.0"$')
            self.assertIn('name = "skipprd-python"\nversion = "17.0.0"', lock)

    def test_normalize_semver_rejects_v_prefix(self):
        module = load_set_version()
        self.assertEqual(module.normalize_semver("1.2.3"), "1.2.3")
        with self.assertRaises(SystemExit):
            module.normalize_semver("v1.2.3")
        with self.assertRaises(SystemExit):
            module.normalize_semver("v0.0.0-rc1")
        with self.assertRaises(SystemExit):
            module.normalize_semver("python-v0.1.0")

    def test_release_bundle_version_rejects_v_prefix(self):
        spec = importlib.util.spec_from_file_location(
            "publish_runtime_plugins",
            ROOT / ".github" / "scripts" / "publish_runtime_plugins.py",
        )
        module = importlib.util.module_from_spec(spec)
        assert spec.loader is not None
        spec.loader.exec_module(module)
        self.assertEqual(module.release_bundle_version("15.13.0"), "15.13.0")
        self.assertIsNone(module.release_bundle_version("v15.13.0"))
        self.assertIsNone(module.release_bundle_version("python-v0.1.0"))
        self.assertIsNone(module.release_bundle_version("latest"))

    def test_darwin_rustc_wrapper_strips_cdylib_flags_from_build_scripts(self):
        spec = importlib.util.spec_from_file_location(
            "darwin_rustc_wrapper", DARWIN_RUSTC_WRAPPER
        )
        module = importlib.util.module_from_spec(spec)
        assert spec.loader is not None
        spec.loader.exec_module(module)
        args = [
            "--crate-name",
            "build_script_build",
            "--crate-type",
            "bin",
            "-C",
            "link-arg=-undefined",
            "-C",
            "link-arg=dynamic_lookup",
            "-C",
            "link-args=-Wl,-install_name,@rpath/skippr.abi3.so",
            "-C",
            "debuginfo=2",
        ]
        self.assertEqual(module.crate_types(args), {"bin"})
        self.assertEqual(
            module.strip_cdylib_link_args(args),
            [
                "--crate-name",
                "build_script_build",
                "--crate-type",
                "bin",
                "-C",
                "debuginfo=2",
            ],
        )
        cdylib = [
            "--crate-type",
            "cdylib",
            "-C",
            "link-arg=-undefined",
        ]
        self.assertEqual(module.crate_types(cdylib), {"cdylib"})
        self.assertTrue(module.KEEP_CRATE_TYPES.intersection(module.crate_types(cdylib)))


if __name__ == "__main__":
    unittest.main()
