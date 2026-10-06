#!/usr/bin/env python3
"""Bike-hire R2 seed writes a single gzip JSONL object of 100_000 events."""

from __future__ import annotations

import gzip
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPTS_DIR = Path(__file__).resolve().parent
MODULE_PATH = SCRIPTS_DIR / "seed_bike_hire_r2.py"
SPEC = importlib.util.spec_from_file_location("seed_bike_hire_r2", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
seed = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(seed)


class SeedBikeHireR2Tests(unittest.TestCase):
    def test_event_count_and_required_fields(self) -> None:
        self.assertEqual(seed.EVENT_COUNT, 100_000)
        self.assertEqual(seed.BUCKET, "skippr-e2e-sample-data")
        self.assertEqual(seed.OBJECT_KEY, "bike-hire/bikehire1.json.gz")

    def test_write_gzip_jsonl_round_trips_schema(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "bikehire1.json.gz"
            seed.write_gzip_jsonl(path, count=3)
            with gzip.open(path, "rt", encoding="utf-8") as handle:
                rows = [json.loads(line) for line in handle]
        self.assertEqual(len(rows), 3)
        for index, row in enumerate(rows, start=1):
            self.assertEqual(row["bike_id"], index)
            self.assertIn(row["event_type"], seed.EVENT_TYPES)
            self.assertIn("rider_id", row)
            self.assertIn("event_date", row)
            self.assertIn("trip", row)


if __name__ == "__main__":
    unittest.main()
