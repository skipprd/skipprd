#!/usr/bin/env python3
"""Engine tags are x.y.z or clobberable x.y.z-beta. Production stays immutable."""

from __future__ import annotations

import unittest
from pathlib import Path

import engine_tag

ROOT = Path(__file__).resolve().parents[2]


class EngineTagTests(unittest.TestCase):
    def test_production_and_beta_shapes(self) -> None:
        self.assertTrue(engine_tag.is_production_engine_tag("18.3.8"))
        self.assertFalse(engine_tag.is_production_engine_tag("0.0.0"))
        self.assertFalse(engine_tag.is_production_engine_tag("18.3.9-beta"))
        self.assertTrue(engine_tag.is_beta_engine_tag("18.3.9-beta"))
        self.assertFalse(engine_tag.is_beta_engine_tag("18.3.9"))
        self.assertFalse(engine_tag.is_beta_engine_tag("18.3.9-rc1"))
        self.assertTrue(engine_tag.is_engine_tag("18.3.9"))
        self.assertTrue(engine_tag.is_engine_tag("18.3.9-beta"))
        self.assertTrue(engine_tag.is_engine_tag("0.0.0"))
        self.assertFalse(engine_tag.is_engine_tag("v18.3.9"))
        self.assertFalse(engine_tag.is_engine_tag("18.3.9-beta.1"))

    def test_next_beta_is_patch_plus_one_of_last_production(self) -> None:
        self.assertEqual(
            engine_tag.next_beta_tag(["18.3.8", "18.3.7", "0.0.0", "18.3.9-beta"]),
            "18.3.9-beta",
        )
        self.assertEqual(engine_tag.next_beta_tag(["1.0.0"]), "1.0.1-beta")
        with self.assertRaises(SystemExit):
            engine_tag.next_beta_tag(["0.0.0", "18.3.9-beta"])

    def test_beta_objects_are_tag_scoped_and_skip_latest(self) -> None:
        self.assertEqual(engine_tag.release_object_prefix("18.3.9-beta"), Path("18.3.9-beta"))
        self.assertIsNone(engine_tag.release_object_prefix("18.3.9"))
        self.assertFalse(engine_tag.should_write_latest_pointer("18.3.9-beta"))
        self.assertTrue(engine_tag.should_write_latest_pointer("18.3.8"))
        self.assertTrue(engine_tag.allow_version_path_clobber("18.3.9-beta"))
        self.assertFalse(engine_tag.allow_version_path_clobber("18.3.8"))

    def test_skill_exists(self) -> None:
        skill = ROOT / ".cursor" / "skills" / "skippr-beta-release" / "SKILL.md"
        self.assertTrue(skill.is_file(), skill)
        text = skill.read_text(encoding="utf-8")
        self.assertIn("name: skippr-beta-release", text)
        self.assertIn("-beta", text)
        self.assertIn("clobber", text)
        self.assertIn("Do **not** run skipprd-docs", text)


if __name__ == "__main__":
    unittest.main()
