#!/usr/bin/env python3
"""Host boundary check must allow shared plugin macros, not connector plugins."""

from __future__ import annotations

import importlib.util
import re
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

    def test_skippr_core_crate_is_deleted(self) -> None:
        self.assertFalse(
            (check_host.REPO_ROOT / "crates" / "skippr-core").exists(),
            "skippr-core is illegal; plugins compile skippr-runtime-sdk only",
        )

    def test_host_tree_has_no_plugins_catalog(self) -> None:
        self.assertEqual(check_host.collect_in_tree_plugin_tree(), [])
        self.assertFalse((check_host.REPO_ROOT / "plugins").exists())

    def test_sdk_sources_do_not_import_skippr_core(self) -> None:
        self.assertEqual(check_host.collect_sdk_skippr_core_imports(), [])

    def test_sdk_tree_parser_flags_skipprd(self) -> None:
        leaked = check_host.packages_named_in_cargo_tree(
            "skipprd v0.1.0\nskippr-runtime-sdk v23.0.0\n"
        )
        self.assertIn("skipprd", leaked)

    def test_sdk_exports_plugin_modules(self) -> None:
        lib = (check_host.SDK_CRATE_DIR / "src" / "lib.rs").read_text(encoding="utf-8")
        for module in (
            "converters",
            "discover",
            "helpers",
            "ingest",
            "metrics",
            "plugins",
            "serdes",
            "cdc_encode",
            "parquet_util",
            "cdc_apply_core",
            "chunker",
            "object_layout",
            "source_sync",
        ):
            self.assertIn(
                f"pub mod {module};",
                lib,
                f"skippr-runtime-sdk must export {module}",
            )
        self.assertNotIn(
            "pub mod source_compat;",
            lib,
            "source_compat is a reexport barrel; import ingest/source_sync/plugins::source_sync",
        )
        self.assertNotIn(
            "pub mod sink_compat;",
            lib,
            "sink_compat is leftover invert naming; BufferChunker lives in chunker",
        )

    def test_host_does_not_define_a_second_buffer_chunker(self) -> None:
        buffer_mod = (check_host.REPO_ROOT / "src" / "buffer" / "mod.rs").read_text(
            encoding="utf-8"
        )
        self.assertNotIn(
            "pub struct BufferChunker",
            buffer_mod,
            "chunk filename codec is skippr_runtime_sdk::chunker::BufferChunker",
        )

    def test_host_does_not_reexport_skippr_hive(self) -> None:
        converters = (check_host.REPO_ROOT / "src" / "converters" / "mod.rs").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("pub mod skippr_hive;", converters)
        self.assertFalse(
            (check_host.REPO_ROOT / "src" / "converters" / "skippr_hive").exists()
        )

    def test_host_does_not_reexport_iceberg_schema(self) -> None:
        cargo = (check_host.REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
        self.assertRegex(
            cargo,
            r"(?m)^skippr-iceberg-writer\s*=",
            "skipprd uses skippr-iceberg-writer types directly",
        )
        self.assertFalse(
            (check_host.REPO_ROOT / "src" / "discover" / "iceberg_schema.rs").exists()
        )

    def test_host_does_not_reexport_sdk_protocol_or_wire(self) -> None:
        runtime_plugins = check_host.REPO_ROOT / "src" / "runtime_plugins"
        self.assertFalse((runtime_plugins / "protocol.rs").exists())
        self.assertFalse((runtime_plugins / "wire.rs").exists())
        mod_rs = (runtime_plugins / "mod.rs").read_text(encoding="utf-8")
        self.assertNotIn("pub mod protocol;", mod_rs)
        self.assertNotIn("pub mod wire;", mod_rs)
        self.assertNotIn("pub use skippr_runtime_sdk::protocol", mod_rs)
        self.assertNotIn("pub use skippr_runtime_sdk::wire", mod_rs)

    def test_precommit_runs_host_dependency_boundaries(self) -> None:
        precommit = (check_host.REPO_ROOT / "scripts" / "precommit.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("test_host_dependency_boundaries.py", precommit)

    def test_agents_release_bump_inputs_are_sdk_and_plugins_only(self) -> None:
        agents = (check_host.REPO_ROOT / "AGENTS.md").read_text(encoding="utf-8")
        self.assertIn("crates/skippr-runtime-sdk", agents)
        self.assertNotIn(
            "protocol, wire, or skippr-runtime-sdk",
            agents,
            "host protocol.rs/wire.rs are not bump inputs; the SDK tree is",
        )

    def test_skipprd_does_not_depend_on_unused_phf(self) -> None:
        cargo = (check_host.REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8")
        self.assertNotRegex(cargo, r"(?m)^phf\s*=")

    def test_converters_export_skippr_hive(self) -> None:
        converters = (
            check_host.SDK_CRATE_DIR / "src" / "converters" / "mod.rs"
        ).read_text(encoding="utf-8")
        self.assertIn(
            "pub mod skippr_hive;",
            converters,
            "SkipprHive lives in skippr-runtime-sdk converters, not a host-only module",
        )

    def test_forbidden_plugin_patterns_ban_deleted_compat_modules(self) -> None:
        self.assertIn("deleted sink_compat import", check_host.FORBIDDEN_PLUGIN_PATTERNS)
        self.assertIn("deleted source_compat import", check_host.FORBIDDEN_PLUGIN_PATTERNS)

    def test_rust_sources_do_not_name_deleted_compat_modules(self) -> None:
        banned = re.compile(r"\b(?:sink_compat|source_compat)\b")
        hits: list[str] = []
        for root in (
            check_host.SDK_CRATE_DIR / "src",
            check_host.REPO_ROOT / "crates" / "skippr-iceberg-writer" / "src",
            check_host.plugin_root() / "plugins",
            check_host.REPO_ROOT / "src",
        ):
            if not root.exists():
                continue
            for path in root.rglob("*.rs"):
                text = path.read_text(encoding="utf-8", errors="ignore")
                if banned.search(text):
                    hits.append(str(path.relative_to(check_host.REPO_ROOT)))
        self.assertEqual(hits, [])


if __name__ == "__main__":
    unittest.main()
