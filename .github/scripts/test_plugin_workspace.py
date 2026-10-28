#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path


SCRIPTS_DIR = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location(
    "plugin_workspace", SCRIPTS_DIR / "plugin_workspace.py"
)
assert SPEC is not None and SPEC.loader is not None
plugin_workspace = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(plugin_workspace)


class PluginWorkspaceTests(unittest.TestCase):
    def test_env_plugins_root_wins(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            crate = root / "plugins" / "data_source" / "file"
            crate.mkdir(parents=True)
            (crate / "Cargo.toml").write_text("[package]\nname = 'file'\n", encoding="utf-8")
            empty = root / "empty"
            empty.mkdir()
            os.environ[plugin_workspace.ENV_PLUGINS_ROOT] = str(root)
            try:
                self.assertEqual(plugin_workspace.resolve_plugin_workspace(empty), root.resolve())
            finally:
                os.environ.pop(plugin_workspace.ENV_PLUGINS_ROOT, None)

    def test_sibling_skipprd_plugins(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            parent = Path(tmp)
            host = parent / "skipprd"
            plugins = parent / "skipprd-plugins"
            host.mkdir()
            crate = plugins / "plugins" / "data_source" / "file"
            crate.mkdir(parents=True)
            (crate / "Cargo.toml").write_text("[package]\nname = 'file'\n", encoding="utf-8")
            os.environ.pop(plugin_workspace.ENV_PLUGINS_ROOT, None)
            self.assertEqual(
                plugin_workspace.resolve_plugin_workspace(host), plugins.resolve()
            )

    def test_missing_plugins_workspace_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            host = Path(tmp) / "skipprd"
            host.mkdir()
            os.environ.pop(plugin_workspace.ENV_PLUGINS_ROOT, None)
            with self.assertRaises(SystemExit) as raised:
                plugin_workspace.resolve_plugin_workspace(host)
            self.assertIn("skipprd-plugins", str(raised.exception))

    def test_host_tree_with_plugins_dir_is_not_the_plugin_workspace(self) -> None:
        """skipprd must not keep a plugins/ catalog after the split."""
        with tempfile.TemporaryDirectory() as tmp:
            host = Path(tmp) / "skipprd"
            leftover = host / "plugins" / "data_source" / "file"
            leftover.mkdir(parents=True)
            (leftover / "Cargo.toml").write_text("[package]\nname = 'file'\n", encoding="utf-8")
            (host / "crates" / "skippr-runtime-sdk").mkdir(parents=True)
            os.environ.pop(plugin_workspace.ENV_PLUGINS_ROOT, None)
            with self.assertRaises(SystemExit):
                plugin_workspace.resolve_plugin_workspace(host)

    def test_leftover_host_plugins_without_sdk_crate_is_not_the_plugin_workspace(self) -> None:
        """A leftover skipprd/plugins tree is illegal even if the SDK crate is missing."""
        with tempfile.TemporaryDirectory() as tmp:
            host = Path(tmp) / "skipprd"
            leftover = host / "plugins" / "data_source" / "file"
            leftover.mkdir(parents=True)
            (leftover / "Cargo.toml").write_text("[package]\nname = 'file'\n", encoding="utf-8")
            (host / "Cargo.toml").write_text(
                '[package]\nname = "skipprd"\nversion = "0.1.0"\n',
                encoding="utf-8",
            )
            os.environ.pop(plugin_workspace.ENV_PLUGINS_ROOT, None)
            with self.assertRaises(SystemExit) as raised:
                plugin_workspace.resolve_plugin_workspace(host)
            self.assertIn("skipprd-plugins", str(raised.exception))
            os.environ[plugin_workspace.ENV_PLUGINS_ROOT] = str(host)
            try:
                with self.assertRaises(SystemExit):
                    plugin_workspace.resolve_plugin_workspace(host)
            finally:
                os.environ.pop(plugin_workspace.ENV_PLUGINS_ROOT, None)

    def test_skipprd_root_from_plugins_sibling(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            parent = Path(tmp)
            host = parent / "skipprd"
            plugins = parent / "skipprd-plugins"
            (host / "crates" / "skippr-runtime-sdk").mkdir(parents=True)
            plugins.mkdir()
            os.environ.pop(plugin_workspace.ENV_SKIPPRD_ROOT, None)
            self.assertEqual(
                plugin_workspace.resolve_skipprd_workspace(plugins), host.resolve()
            )


if __name__ == "__main__":
    unittest.main()
