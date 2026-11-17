---
title: Materialised views
description: Keyed running aggregates over committed WAL, stored in Skippr Store stream_state, rebuilt when you change the definition.
---

# Materialised views

A Skipprd materialised view is a **keyed running aggregate** over committed WAL data. Skipprd keeps the measures in Skippr Store (`stream_state`), in the same transaction as the WAL cursor, so a crash does not leave a total that the log has not caught up to.

This is production ingest state. It is not a warehouse `CREATE MATERIALIZED VIEW`, and it is not a second Iceberg table. Iceberg files on your object store are the durable landing of the stream. `stream_state` is the live keyed state next to that landing.

## Configure

Set `transform.compute` on the pipeline. `CREATE TABLE … GROUP BY` journals a keyed table:

```yaml
pipelines:
  invoices:
    data_source: data_sources.pg
    data_sink: data_sinks.lake
    transform:
      compute:
        - name: invoices.customer_totals
          type: sql
          value: |
            CREATE TABLE invoices.customer_totals AS
            SELECT customer_id, SUM(amount) AS invoice_total, COUNT(*) AS invoice_count
            FROM invoices.orders
            GROUP BY customer_id
```

Python:

```python
from skippr import ComputeItemSql, Config, Pipeline, Transform

cfg = Config.load("skippr.yml")
cfg.pipeline(
    "invoices",
    Pipeline(
        data_source=cfg.get_data_source("pg"),
        data_sink=cfg.get_data_sink("lake"),
        transform=Transform(
            compute=[
                ComputeItemSql(
                    name="invoices.customer_totals",
                    value="""CREATE TABLE invoices.customer_totals AS
                    SELECT customer_id, SUM(amount) AS invoice_total, COUNT(*) AS invoice_count
                    FROM invoices.orders
                    GROUP BY customer_id""",
                )
            ],
        ),
    ),
)
cfg.save()
```

`type: copy` cannot set `compute` sql or sdl. `type: py` is allowed (`path` + `handler`): the worker maps JSON objects and the copy identity is the transformed line. A handler error deadletters that record as `PYTHON_COMPUTE`. Changing `path`, `handler`, or a sibling `.py` changes the journaled digest (`DROP` then `CREATE`). Sync admit fails closed until then. Ingest deadletters those batches as `PYTHON_COMPUTE` if disk path, handler, or digest does not match the journal. A SQL/SDL transform error fails the ingest batch and does not advance source offsets.

## What Skipprd does

After a WAL segment is committed, a single-worker processor reads those immutable rows and updates `stream_state`:

| CDC event | Measure |
|---|---|
| insert | add |
| update | apply `new − last_row` for that key (missing last-row is treated as insert) |
| delete | subtract last-row (missing last-row is a no-op) |
| primary-key change | delete-old + insert-new, using leftover entity columns when present |

One Skippr Store transaction writes measures, last-row images, the entity index, dedup keys, and the WAL cursor (`seq`, `segment_id`, `slice_ordinal`, `row_index`). Compaction does **not** drop a WAL segment until the processor's cursor has passed it. Ingest is not blocked; WAL retention waits on processor lag.

Changing keys or measures is a new catalog object. `ALTER COMPUTE` (or View / keyed / windowed `ALTER TABLE` sugar) writes the new entry and rebuilds that item from Iceberg ∪ WAL. Latest and stream-table joins are `DROP` then `CREATE`.

## How this relates to SQL

[`skipprd query`](/cli/query) `SELECT`s the pipeline namespace (Iceberg ∪ live WAL) and registered `pipeline.compute_name` tables. You `SELECT * FROM invoices.customer_totals`.

Query the stream as usual:

```bash
skipprd query --plain --sql "SELECT customer_id, sum(amount) FROM invoices.orders GROUP BY customer_id"
```

That GROUP BY is computed at query time over Iceberg ∪ WAL. The materialised totals in `stream_state` are the ingest-side checkpoint Skipprd keeps so keyed state survives crash and compact.

Processor output into sinks (publishing those keys as tables) is not this path.

## Skippr Store

| Family | Role for materialised views |
|---|---|
| `catalog` | Live WAL segments the processor is allowed to read |
| `stream_state` | Measures, last-row, entity index, cursor in one commit |
| `offsets` | Source resume. Independent of the processor cursor |
| `schema` | Pipeline fields the keys and measures name |

One writer process. Readers pin. See [Skippr Store](/configuration/skippr-store).

## Next steps

- [Transforms](/configuration/transforms)
- [Stream SQL](/concepts/stream-sql)
- [Skippr Store](/configuration/skippr-store)
- [How Skipprd works](/concepts/how-it-works)
