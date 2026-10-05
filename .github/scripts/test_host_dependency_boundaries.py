#!/usr/bin/env python3
"""Host boundary check must allow shared plugin macros, not connector plugins."""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

SCRIPTS_DIR = Path(__file__).resolve().parent
MODULE_PATH = SCRIPTS_DIR / "check_host_dependency_boundaries.py"
SPEC = importlib.util.spec_from_file_location("check_host_dependency_boundaries", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
check_host = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(check_host)


class HostDependencyBoundaryTests(unittest.TestCase):
    def test_plugin_macros_are_host_shared_not_connectors(self) -> None:
        self.assertIn("skippr-plugin-macros", check_host.ALLOWED_HOST_PLUGIN_PREFIX_PACKAGES)
        self.assertTrue("skippr-plugin-data-sink-postgres".startswith("skippr-plugin-"))
        self.assertNotIn(
            "skippr-plugin-data-sink-postgres",
            check_host.ALLOWED_HOST_PLUGIN_PREFIX_PACKAGES,
        )


if __name__ == "__main__":
    unittest.main()
