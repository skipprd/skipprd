from pathlib import Path

from skippr import Migration


def test_migration_write_emits_sql_file(tmp_path: Path):
    path = Migration(
        "20261020_rename_region",
        up="ALTER TABLE bikehire.trips RENAME COLUMN region TO region_code;",
        down="ALTER TABLE bikehire.trips RENAME COLUMN region_code TO region;",
    ).write(tmp_path)
    text = path.read_text(encoding="utf-8")
    assert path.name == "20261020_rename_region.sql"
    assert "RENAME COLUMN region TO region_code" in text
    assert "-- migrate:down" in text
    assert "RENAME COLUMN region_code TO region" in text
