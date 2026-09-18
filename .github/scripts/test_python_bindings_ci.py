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
        self.assertIn("cargo test -p skipprd --lib", text)
        self.assertIn("skippr-connect-gen", text)
        self.assertNotIn("cargo test --workspace", text)

    def test_python_script_builds_wheel_and_runs_session_tests(self):
        script = TEST_PYTHON.read_text(encoding="utf-8")
        self.assertIn("maturin develop", script)
        self.assertIn("python/tests", script)
        self.assertIn("maturin build", script)
        self.assertIn("Darwin", script)
        self.assertIn("CARGO_TARGET_DIR", script)
        self.assertIn("maturin", script)

    def test_ci_builds_on_skippr_cloud_runners(self):
        text = CI.read_text(encoding="utf-8")
        self.assertIn("skippr-linux-x64-16", text)
        self.assertIn("skippr-darwin-arm64-8", text)
        self.assertNotIn("ubuntu-latest", text)
        self.assertNotIn("macos-latest", text)
        self.assertNotIn("depot", text)
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
        publish = text.split("python-publish:", 1)[1]
        self.assertNotIn("environment:", publish)
        self.assertIn("attestations: false", publish)
        self.assertIn("skippr-linux-x64-16", publish)
        self.assertIn("refs/heads/main", publish)
        self.assertIn("refs/tags/python-v", publish)
        self.assertIn("python_wheel_version.py", publish)
        self.assertNotIn("github.ref != 'refs/tags/v0.0.0'", publish)
        self.assertNotRegex(publish, r"startsWith\(github\.ref, 'refs/tags/v'\)")
        self.assertIn("skipprd/cloud", text)
        self.assertIn("path: cloud", text)
        self.assertIn("path: skipprd", text)
        self.assertIn("SKIPPR_CLOUD_CHECKOUT_TOKEN", text)

    def test_release_workflow_registers_pypi_project_skippr(self):
        text = (ROOT / "docs" / "docs" / "maintainers" / "release-workflow.md").read_text(
            encoding="utf-8"
        )
        self.assertIn("- Project: `skippr`", text)
        self.assertIn("- Workflow name: `ci.yml`", text)
        self.assertNotIn("- Project: `skipprd`", text)

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
        self.assertIn(".githooks/pre-commit", installer)

    def test_python_semver_is_independent_of_engine_tags(self):
        module = load_wheel_version()
        self.assertEqual(module.python_semver(), "0.1.0")
        self.assertTrue(module.should_publish("0.1.0", published=False))
        self.assertFalse(module.should_publish("0.1.0", published=True))
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
                    "v9.8.7",
                ]
            )
            pyproject = (root / "pyproject.toml").read_text(encoding="utf-8")
            python_cargo = (root / "python" / "Cargo.toml").read_text(encoding="utf-8")
            lock = (root / "Cargo.lock").read_text(encoding="utf-8")
            self.assertRegex((root / "Cargo.toml").read_text(encoding="utf-8"), r'(?m)^version = "9\.8\.7"$')
            self.assertRegex(pyproject, r'(?m)^version = "0\.1\.0"$')
            self.assertRegex(python_cargo, r'(?m)^version = "0\.1\.0"$')
            self.assertIn('name = "skipprd-python"\nversion = "0.1.0"', lock)

    def test_normalize_semver_strips_v_prefix(self):
        module = load_set_version()
        self.assertEqual(module.normalize_semver("v1.2.3"), "1.2.3")
        self.assertEqual(module.normalize_semver("1.2.3"), "1.2.3")
        with self.assertRaises(SystemExit):
            module.normalize_semver("v0.0.0-rc1")


if __name__ == "__main__":
    unittest.main()
