#!/usr/bin/env python3
"""Bike-hire R2 seed matches soda's 5_100_000 mixed-size, mixed-shape objects."""

from __future__ import annotations

import gzip
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPTS_DIR = Path(__file__).resolve().parent
REPO = SCRIPTS_DIR.parents[1]
MODULE_PATH = SCRIPTS_DIR / "seed_bike_hire_r2.py"
SPEC = importlib.util.spec_from_file_location("seed_bike_hire_r2", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
seed = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = seed
SPEC.loader.exec_module(seed)


class SeedBikeHireR2Tests(unittest.TestCase):
    def test_canonical_count_matches_soda(self) -> None:
        self.assertEqual(seed.CANONICAL_EVENT_COUNT, 5_100_000)
        self.assertEqual(seed.event_total(seed.CHAOS_SPECS), 5_100_000)
        soda = (REPO / "soda" / "bike_hire_many.yml").read_text(encoding="utf-8")
        self.assertIn("row_count = 5100000", soda)
        chaos = (REPO / "soda" / "chaos_mode.yml").read_text(encoding="utf-8")
        self.assertIn("row_count = 5100000", chaos)

    def test_chaos_layout_covers_size_and_shape(self) -> None:
        keys = [spec.relative_key for spec in seed.CHAOS_SPECS]
        self.assertTrue(any(spec.gzip and spec.event_count >= 100_000 for spec in seed.CHAOS_SPECS))
        self.assertTrue(any(not spec.gzip and spec.event_count <= 50 for spec in seed.CHAOS_SPECS))
        self.assertTrue(any(spec.shape == "v1" for spec in seed.CHAOS_SPECS))
        self.assertTrue(any(spec.shape == "v2" for spec in seed.CHAOS_SPECS))
        self.assertTrue(any(spec.shape == "flat" for spec in seed.CHAOS_SPECS))
        self.assertTrue(any(key.startswith("tiny/") for key in keys))
        self.assertTrue(any(key.startswith("bulk/") for key in keys))
        self.assertEqual(len(keys), len(set(keys)))

    def test_evolve_layout_is_two_wave_backward_compatible(self) -> None:
        self.assertEqual(seed.event_total(seed.EVOLVE_SPECS), seed.EVOLVE_EVENT_COUNT)
        self.assertEqual(seed.EVOLVE_SPECS[0].shape, "v1")
        self.assertEqual(seed.EVOLVE_SPECS[1].shape, "v2")
        v1 = seed.event_for_index(1, "v1")
        v2 = seed.event_for_index(1, "v2")
        self.assertNotIn("firmware_revision", v1)
        self.assertEqual(v2["firmware_revision"], "2.0")
        self.assertNotIn("modem", v1["hardware"])
        self.assertEqual(v2["hardware"]["modem"], "lte")
        self.assertTrue(set(v1).issubset(set(v2)))

    def test_write_specs_round_trips_mixed_objects(self) -> None:
        specs = (
            seed.ObjectSpec("bulk/part-00.json.gz", 2, "v1", True),
            seed.ObjectSpec("tiny/2020/01/01/t-0000.json", 1, "flat", False),
            seed.ObjectSpec("evolve/v2.json.gz", 1, "v2", True),
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            written = seed.write_specs(root, specs)
            self.assertEqual(len(written), 3)
            bulk = written[0]
            with gzip.open(bulk, "rt", encoding="utf-8") as handle:
                rows = [json.loads(line) for line in handle]
            self.assertEqual([row["bike_id"] for row in rows], [1, 2])
            self.assertNotIn("firmware_revision", rows[0])
            tiny = json.loads((root / "tiny/2020/01/01/t-0000.json").read_text(encoding="utf-8").splitlines()[0])
            self.assertIn("trip_id", tiny)
            with gzip.open(root / "evolve/v2.json.gz", "rt", encoding="utf-8") as handle:
                evolved = json.loads(handle.readline())
            self.assertEqual(evolved["firmware_revision"], "2.0")
            self.assertEqual(evolved["bike_id"], 4)


if __name__ == "__main__":
    unittest.main()
