#!/usr/bin/env python3
"""Engine tags are x.y.z or immutable x.y.z-beta-N. Production stays immutable."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))
import engine_tag

ROOT = Path(__file__).resolve().parents[2]


class EngineTagTests(unittest.TestCase):
    def test_production_and_beta_shapes(self) -> None:
        self.assertTrue(engine_tag.is_production_engine_tag("18.3.8"))
        self.assertFalse(engine_tag.is_production_engine_tag("0.0.0"))
        self.assertFalse(engine_tag.is_production_engine_tag("18.3.9-beta-1"))
        self.assertFalse(engine_tag.is_production_engine_tag("18.3.9-beta"))
        self.assertTrue(engine_tag.is_beta_engine_tag("18.3.9-beta-1"))
        self.assertFalse(engine_tag.is_beta_engine_tag("18.3.9-beta"))
        self.assertFalse(engine_tag.is_beta_engine_tag("18.3.9-beta-0"))
        self.assertFalse(engine_tag.is_beta_engine_tag("18.3.9-beta-01"))
        self.assertFalse(engine_tag.is_beta_engine_tag("18.3.9"))
        self.assertFalse(engine_tag.is_beta_engine_tag("18.3.9-rc1"))
        self.assertTrue(engine_tag.is_engine_tag("18.3.9"))
        self.assertTrue(engine_tag.is_engine_tag("18.3.9-beta-1"))
        self.assertTrue(engine_tag.is_engine_tag("0.0.0"))
        self.assertFalse(engine_tag.is_engine_tag("v18.3.9"))
        self.assertFalse(engine_tag.is_engine_tag("18.3.9-beta"))
        self.assertFalse(engine_tag.is_engine_tag("18.3.9-beta.1"))

    def test_next_beta_increments_n_monotonically(self) -> None:
        self.assertEqual(
            engine_tag.next_beta_tag(["18.3.8", "18.3.7", "0.0.0"]),
            "18.3.9-beta-1",
        )
        self.assertEqual(
            engine_tag.next_beta_tag(["18.3.8", "18.3.9-beta-1", "18.3.9-beta-2"]),
            "18.3.9-beta-3",
        )
        self.assertEqual(
            engine_tag.next_beta_tag(["18.3.8", "18.3.9-beta-1", "18.3.9-beta-5"]),
            "18.3.9-beta-6",
        )
        self.assertEqual(engine_tag.next_beta_tag(["1.0.0"]), "1.0.1-beta-1")
        self.assertEqual(
            engine_tag.next_beta_tag(["18.3.8", "18.3.9-beta"]),
            "18.3.9-beta-1",
        )
        with self.assertRaises(SystemExit):
            engine_tag.next_beta_tag(["0.0.0", "18.3.9-beta-1"])

    def test_pep440_beta_is_unique_prerelease(self) -> None:
        self.assertEqual(engine_tag.pep440_version("18.3.10-beta-1"), "18.3.10b1")
        self.assertEqual(engine_tag.pep440_version("18.3.10-beta-12"), "18.3.10b12")
        self.assertEqual(engine_tag.pep440_version("18.3.10"), "18.3.10")

    def test_beta_objects_are_tag_scoped_immutable_and_skip_latest(self) -> None:
        self.assertEqual(
            engine_tag.release_object_prefix("18.3.9-beta-1"), Path("18.3.9-beta-1")
        )
        self.assertIsNone(engine_tag.release_object_prefix("18.3.9"))
        self.assertFalse(engine_tag.should_write_latest_pointer("18.3.9-beta-1"))
        self.assertTrue(engine_tag.should_write_latest_pointer("18.3.8"))
        self.assertFalse(engine_tag.allow_version_path_clobber("18.3.9-beta-1"))
        self.assertFalse(engine_tag.allow_version_path_clobber("18.3.8"))

    def test_bash_regex_is_shared_with_release_action(self) -> None:
        action = (
            ROOT / ".github" / "actions" / "rust-build-release" / "action.yaml"
        ).read_text(encoding="utf-8")
        self.assertIn(engine_tag.ENGINE_TAG_BASH_RE, action)

    def test_skill_is_monotonic_not_clobberable(self) -> None:
        skill = ROOT / ".cursor" / "skills" / "skippr-beta-release" / "SKILL.md"
        self.assertTrue(skill.is_file(), skill)
        text = skill.read_text(encoding="utf-8")
        self.assertIn("name: skippr-beta-release", text)
        self.assertIn("-beta-N", text)
        self.assertIn("next_beta_tag", text)
        self.assertNotIn("git tag -f", text)
        self.assertNotIn("Delete+re-push", text)
        self.assertNotIn("re-clobbering", text)
        self.assertIn("Do **not** run skipprd-docs", text)


if __name__ == "__main__":
    unittest.main()
