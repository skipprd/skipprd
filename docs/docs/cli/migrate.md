---
title: skipprd migrate
description: Apply SQL schema migrations next to skippr.yml without stopping the pipeline. The journal lives in Skippr Store.
---

# skipprd migrate

`skipprd migrate` applies `migrations/*.sql` next to `skippr.yml`. Each file is one atomic `ALTER TABLE` batch. Skipprd records the file in the Skippr Store **journal** family so a retry is idempotent: the same id and checksum is a no-op, a changed file with the same id fails closed.

Python authors those files; the engine never loads `.py`. `Session.migrate()` is the same path as the CLI.

You do not stop the pipeline. Apply is in-stream. `DISABLE PIPELINE` still freezes ingest; it is not the ALTER gate.

## Usage

```bash
skipprd migrate --pipeline <NAME> [--down] [--config <path>] [--log [LEVEL]]
```

## Options

| Flag | Default | Description |
|---|---|---|
| `-p`, `--pipeline <NAME>` | None | Pipeline whose `migrations/` apply. Required in practice when several pipelines share a config. |
| `--down` | Off | Apply `-- migrate:down` instead of `-- migrate:up`. Only Rename and Promote may have a down section. |

See [CLI overview](/cli/overview) for global flags (`--config`, `--store-type`, `--wal-storage`, …).

## Examples

Apply pending files, then sync:

```bash
skipprd migrate --pipeline bikehire
skipprd sync --pipeline bikehire --once
```

Roll back a Rename or Promote with `--down`:

```bash
skipprd migrate --pipeline bikehire --down
```

## Author a file

```python
from skippr import Migration

Migration(
    "20261020_rename_region",
    up="ALTER TABLE bikehire.trips RENAME COLUMN region TO region_code;",
    down="ALTER TABLE bikehire.trips RENAME COLUMN region_code TO region;",
).write("migrations")
```

Files use `-- migrate:up` (optional) and `-- migrate:down` (optional, Rename/Promote only).

```sql
-- migrate:up
ALTER TABLE bikehire.trips RENAME COLUMN region TO region_code;

-- migrate:down
ALTER TABLE bikehire.trips RENAME COLUMN region_code TO region;
```

`down()` is only valid for Rename and Promote. Drop and Merge fail closed if a `-- migrate:down` section is present.

## Query ALTER

`skipprd query --sql "ALTER TABLE …"` uses the same apply path. Skipprd journals it as `query-<checksum>` in Skippr Store, so repeating the same statement is safe.

Prefer files in `migrations/` for anything you want in git. Use query ALTER for a one-off.

## What apply does

For each pending file, Skipprd:

1. Loads the journal from Skippr Store under the writer lock.
2. Rewrites skippr metadata (`schema` family) and live WAL so later ingest uses the new names and types.
3. Applies native DDL on destinations that advertise it (Iceberg: SkipprLake, Athena Iceberg, DuckDB; plus Snowflake, Postgres, BigQuery, Redshift, MotherDuck). Glue/Hive and ClickHouse fail closed.
4. Commits the journal row (id, checksum, phase). A crash mid-apply resumes from that phase; it does not double-apply a finished file.

Iceberg keeps the field id, so existing Parquet stays readable after rename.

Statements: `RENAME COLUMN`, `MERGE COLUMN`, `DROP COLUMN`, `ALTER COLUMN … TYPE`. Nested `TO` paths such as `detail.region` are valid. See [ALTER operations](/operations/alter) for the matrix and fail-closed cases.

## Skippr Store

Migrations do not use a second KV. Journal, schema, and WAL catalog share Skippr Store with ingest. One writer. See [Skippr Store](/configuration/skippr-store).

## Next steps

- [ALTER operations](/operations/alter)
- [Python](/python)
- [skipprd query](/cli/query)
- [Skippr Store](/configuration/skippr-store)
