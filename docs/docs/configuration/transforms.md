---
title: Transforms
description: Split records into tables, partition output by field or time, flatten nested objects, and sort rows for faster queries.
---

# Transforms

A transform shapes records between the source and the destination. Use one to split a mixed stream into separate tables, partition files by a field or by time, flatten nested JSON into columns, or sort rows so queries scan less data.

Transforms are set per pipeline under `transform:`. Every key is optional.

## Set a transform

This transform splits events into one table per `event_type`, partitions each table by day, and sorts rows by `customer_id`:

::: code-group

```python [Python]
from skippr import Config, Pipeline, Transform

cfg = Config.load("skippr.yml")
cfg.pipeline(
    "events",
    Pipeline(
        data_source=cfg.get_data_source("events"),
        data_sink=cfg.get_data_sink("lake"),
        transform=Transform(
            namespace_fields="event_type",
            batch_time_fields="created_at",
            batch_time_unit="day",
            batch_order_fields="customer_id",
        ),
    ),
)
cfg.save()
```

```yaml [YAML]
pipelines:
  events:
    data_source: data_sources.events
    data_sink: data_sinks.lake
    transform:
      namespace_fields: event_type
      batch_time_fields: created_at
      batch_time_unit: day
      batch_order_fields: customer_id
```

:::

`skipprd connect` does not write transforms; set them in YAML or Python. Run `skipprd discover --pipeline events` after you change `namespace_fields`, because it changes which tables exist.

## Options

| Key | Environment fallback | Default | Description |
|---|---|---|---|
| `namespace_fields` | `TRANSFORM_NAMESPACE_FIELDS` | none (one table) | Fields that split records into tables. Each distinct combination of values becomes its own table and schema. Comma-separated. |
| `batch_partition_fields` | `TRANSFORM_BATCH_PARTITION_FIELDS` | none | Hive-style partition columns. Comma-separated; nested paths use dots, as in `country,product.category`. |
| `partition_allowed_values` | `TRANSFORM_PARTITION_ALLOWED_VALUES` | none | Comma-separated values allowed as partition values. Requires `batch_partition_fields`. |
| `batch_time_fields` | `TRANSFORM_BATCH_TIME_FIELDS` | none | Timestamp fields used for time partitions. Comma-separated; Skipprd uses the first one present in each record. |
| `batch_time_unit` | `TRANSFORM_BATCH_TIME_UNIT` | none | Time-partition depth: `year`, `month`, `day`, `hour`, or `minute`. Requires `batch_time_fields`. |
| `time_partition_prefix` | `TRANSFORM_TIME_PARTITION_PREFIX` | none | Text added before each time-partition folder name. |
| `flatten_events` | `TRANSFORM_FLATTEN_EVENTS` | `false` | Turn nested objects into top-level columns with dotted names. |
| `record_field_path` | `TRANSFORM_RECORD_FIELD_PATH` | none (each object is a record) | Path to an array of records inside each source object. |
| `inject_fields` | none | none | Fixed field names and values added to every record. |
| `batch_order_fields` | `TRANSFORM_BATCH_ORDER_FIELDS` | none (unsorted) | Columns to sort rows by inside each Parquet file. Comma-separated. |
| `enable_single_quote_parsing` | none | `false` | Accept JSON that wraps strings in single quotes. |
| `enable_unicode_parsing` | none | `false` | Accept JSON with unescaped Unicode. |

A key in `skippr.yml` beats its environment fallback. Flags accept `true`/`false`, `yes`/`no`, or `1`/`0`.

## Split records into tables

Set `namespace_fields` when one source carries several kinds of record. With `namespace_fields: event_type`, records with `event_type: signup` and `event_type: purchase` land in separate tables, each with its own schema. Use several fields, such as `category,sub_category`, to split on their combination.

Without `namespace_fields`, all records from a source share one table, apart from sources that already produce one table per object, such as database tables.

## Partition output

Partitions organise files into folders so query engines can skip folders a filter rules out.

- **By field:** `batch_partition_fields: country` writes `p_country=GB/`, `p_country=US/`, and so on. To cap the number of partitions, list the values you want in `partition_allowed_values`; other values are not used as partition values.
- **By time:** `batch_time_fields: created_at` with `batch_time_unit: day` writes `year=2026/month=10/day=9/` folders. Each unit includes the coarser ones above it. `time_partition_prefix: p_` changes those to `p_year=2026/p_month=10/p_day=9/`.

Setting `batch_time_unit` without `batch_time_fields`, or `partition_allowed_values` without `batch_partition_fields`, stops the run with `Config dependency missing: ...`.

Partition folders apply to destinations that write files, such as S3, GCS, Azure Blob, Local file, and Athena. Warehouse destinations load tables directly; see each connector page.

## Flatten nested objects

With `flatten_events: true`, a record like:

```json
{ "id": 7, "contact": { "name": "Ada", "email": "ada@example.com" } }
```

lands with columns `id`, `contact.name`, and `contact.email` instead of an `id` column and a nested `contact` struct. Flatten when your destination or BI tool handles flat columns better than nested ones.

## Read records nested in an array

Some APIs and exports wrap records in an envelope:

```json
{ "page": 1, "data": [ { "id": 1 }, { "id": 2 } ] }
```

Set `record_field_path` to the array so each element becomes a record. Without it, Skipprd treats the whole object as one record.

## Add fixed fields

`inject_fields` adds the same fields to every record before ingest, for example to tag where data came from:

```yaml
transform:
  inject_fields:
    source_system: billing
    region: eu-west-1
```

## Sort rows for faster queries

`batch_order_fields` sorts rows inside each Parquet file before Skipprd writes it. Parquet stores the minimum and maximum of every column for each row group, so after sorting by `bar`, a query such as `SELECT * FROM foo WHERE bar = 4` can skip every row group whose range excludes `4`. Unsorted, matching rows are scattered and the query may scan the whole file.

- Fields apply in order. The first gives the strongest clustering, so put the column you filter on most first.
- One or two fields is usually best. Each extra field helps queries that filter on all of them but weakens clustering on the first.
- Names are output column names. A field missing from a table is ignored for that table. If none match, that table is written unsorted.
- At the end of a run, Skipprd warns `batch_order_fields never matched any namespace during this run: [...]` for fields that matched no table, which usually means a typo.

While sorting is on, Skipprd sizes row groups automatically, between about 16 MiB and 64 MiB uncompressed (25,000 to 500,000 rows), based on row width and how many distinct values the leading sort column has. You do not need to tune it.

## Next steps

- [Destinations](/configuration/output) — where transformed records land.
- [Schema discovery and evolution](/concepts/schema) — how new fields and tables are detected.
- [Pipelines](/configuration/pipeline) — the other pipeline keys.
