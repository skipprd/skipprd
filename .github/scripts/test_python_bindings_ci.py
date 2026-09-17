#!/usr/bin/env python3
"""CI must build the skipprd wheel and run Session tests."""

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


def load_set_version():
    spec = importlib.util.spec_from_file_location("set_root_package_version", SET_VERSION)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


class PythonBindingsCiTests(unittest.TestCase):
    def test_ci_runs_python_binding_script(self):
        text = CI.read_text(encoding="utf-8")
        self.assertIn("scripts/test-python.sh", text)

    def test_python_script_builds_wheel_and_runs_session_tests(self):
        script = TEST_PYTHON.read_text(encoding="utf-8")
        self.assertIn("maturin develop", script)
        self.assertIn("python/tests/test_session.py", script)
        self.assertIn("maturin build", script)

    def test_ci_builds_on_skippr_cloud_runners(self):
        text = CI.read_text(encoding="utf-8")
        self.assertIn("skippr-linux-x64-16", text)
        self.assertIn("skippr-darwin-arm64-8", text)
        self.assertNotIn("ubuntu-latest", text)
        self.assertNotIn("macos-latest", text)

    def test_ci_publishes_wheels_with_pypi_oidc(self):
        text = CI.read_text(encoding="utf-8")
        self.assertEqual(CI.name, "ci.yml")
        self.assertIn("python-publish:", text)
        self.assertIn("pypa/gh-action-pypi-publish", text)
        self.assertIn("id-token: write", text)
        self.assertIn("upload-artifact", text)
        self.assertIn("set_root_package_version.py", text)
        self.assertIn("github.ref != 'refs/tags/v0.0.0'", text)
        self.assertNotIn("PYPI_API_TOKEN", text)
        self.assertNotIn("TWINE_PASSWORD", text)
        self.assertIn("tags:", text)
        publish = text.split("python-publish:", 1)[1]
        self.assertNotIn("environment:", publish)
        self.assertIn("attestations: false", publish)
        self.assertIn("skippr-linux-x64-16", publish)
        self.assertIn("skipprd/cloud", text)
        self.assertIn("path: cloud", text)
        self.assertIn("path: skipprd", text)
        self.assertIn("SKIPPR_CLOUD_CHECKOUT_TOKEN", text)

    def test_pyproject_declares_license_and_readme(self):
        text = (ROOT / "pyproject.toml").read_text(encoding="utf-8")
        self.assertRegex(text, r'(?m)^readme\s*=')
        self.assertIn("LICENSE", text)

    def test_python_wheel_is_abi3_py310(self):
        text = PYTHON_CARGO.read_text(encoding="utf-8")
        self.assertIn("abi3-py310", text)

    def test_set_root_package_version_stamps_python_from_v_tag(self):
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
            self.assertRegex(pyproject, r'(?m)^version = "9\.8\.7"$')
            self.assertIn('name = "skipprd-python"', python_cargo)
            self.assertRegex(python_cargo, r'(?m)^version = "9\.8\.7"$')
            self.assertIn('name = "skipprd-python"\nversion = "9.8.7"', lock)

    def test_normalize_semver_strips_v_prefix(self):
        module = load_set_version()
        self.assertEqual(module.normalize_semver("v1.2.3"), "1.2.3")
        self.assertEqual(module.normalize_semver("1.2.3"), "1.2.3")
        with self.assertRaises(SystemExit):
            module.normalize_semver("v0.0.0-rc1")


if __name__ == "__main__":
    unittest.main()
