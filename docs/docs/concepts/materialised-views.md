---
title: Materialised views
description: Keyed running aggregates over committed WAL, stored in Skippr Store stream_state, rebuilt when you change the definition.
---

# Materialised views

A Skipprd materialised view is a **keyed running aggregate** over committed WAL data. Skipprd keeps the measures in Skippr Store (`stream_state`), in the same transaction as the WAL cursor, so a crash does not leave a total that the log has not caught up to.

This is production ingest state. It is not a warehouse `CREATE MATERIALIZED VIEW`, and it is not a second Iceberg table. Iceberg files on your object store are the durable landing of the stream. `stream_state` is the live keyed state next to that landing.

## Configure

Set `transform.aggregates` on the pipeline. Each entry needs a stable `id`, key columns, and at least one measure (`sum` or `count`):

```yaml
pipelines:
  invoices:
    data_source: data_sources.pg
    data_sink: data_sinks.lake
    transform:
      aggregates:
        - id: customer_invoice_totals
          key_fields: [customer_id]
          measures:
            invoice_total: { sum: amount }
            invoice_count: { count: "*" }
```

Python:

```python
from skippr import (
    AggregateMeasure,
    Config,
    Pipeline,
    Transform,
    TransformAggregate,
)

cfg = Config.load("skippr.yml")
cfg.pipeline(
    "invoices",
    Pipeline(
        data_source=cfg.get_data_source("pg"),
        data_sink=cfg.get_data_sink("lake"),
        transform=Transform(
            aggregates=[
                TransformAggregate(
                    id="customer_invoice_totals",
                    key_fields=["customer_id"],
                    measures={
                        "invoice_total": AggregateMeasure(sum="amount"),
                        "invoice_count": AggregateMeasure(count="*"),
                    },
                )
            ],
        ),
    ),
)
cfg.save()
```

`type: copy` cannot set `aggregates`. A transform error fails the ingest batch and does not advance source offsets.

## What Skipprd does

After a WAL segment is committed, a single-worker processor reads those immutable rows and updates `stream_state`:

| CDC event | Measure |
|---|---|
| insert | add |
| update | apply `new − last_row` for that key (missing last-row is treated as insert) |
| delete | subtract last-row (missing last-row is a no-op) |
| primary-key change | delete-old + insert-new, using leftover entity columns when present |

One Skippr Store transaction writes measures, last-row images, the entity index, dedup keys, and the WAL cursor (`seq`, `segment_id`, `slice_ordinal`, `row_index`). Compaction does **not** drop a WAL segment until the processor's cursor has passed it. Ingest is not blocked; WAL retention waits on processor lag.

Changing an aggregate's `id`, keys, or measures changes the config hash. Skipprd deletes that aggregate's prefix and rebuilds it by replaying the WAL from the start.

## How this relates to SQL

[`skipprd query`](/cli/query) `SELECT`s the pipeline namespace: Iceberg ∪ live WAL after `transform.ops`. Aggregates live in Skippr Store. They are not extra names in `skipprd query`. You do not `SELECT * FROM customer_invoice_totals` as a catalog name.

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
