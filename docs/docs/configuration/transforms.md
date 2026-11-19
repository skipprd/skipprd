---
title: Transforms
description: Split records into tables, partition output, flatten nested objects, and declare compute SQL that Skipprd journals in Skippr Store.
---

# Transforms

A transform shapes records between the source and the destination. Use one to split a mixed stream into separate tables, partition files by a field or by time, flatten nested JSON into columns, sort rows so queries scan less data, filter or derive columns in memory, or keep keyed running aggregates in Skippr Store.

Transforms are set per pipeline under `transform:`. Every key is optional.

## Set a transform

This transform splits events into one table per `event_type`, partitions each table by day, and sorts rows by `customer_id`:

::: code-group

```python [Python]
from skippr import BatchTimeUnit, Config, Pipeline, Transform

cfg = Config.load("skippr.yml")
cfg.pipeline(
    "events",
    Pipeline(
        data_source=cfg.get_data_source("events"),
        data_sink=cfg.get_data_sink("lake"),
        transform=Transform(
            namespace_fields="event_type",
            batch_time_fields="created_at",
            batch_time_unit=BatchTimeUnit.DAY,
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
| `flatten_events` | `TRANSFORM_FLATTEN_EVENTS` | `false` | Turn nested objects into top-level columns with underscore names. |
| `record_field_path` | `TRANSFORM_RECORD_FIELD_PATH` | none (each object is a record) | Path to an array of records inside each source object. |
| `inject_fields` | none | none | Fixed field names and values added to every record after `type: py`. |
| `batch_order_fields` | `TRANSFORM_BATCH_ORDER_FIELDS` | none (unsorted) | Columns to sort rows by inside each Parquet file. Comma-separated. |
| `enable_single_quote_parsing` | none | `false` | Accept JSON that wraps strings in single quotes. |
| `enable_unicode_parsing` | none | `false` | Accept JSON with unescaped Unicode. |
| `event_time` | `TRANSFORM_BATCH_TIME_FIELDS` | none | Event-time column (YAML alias `batch_time_fields`). File/Iceberg partitions use it. Required for windowed `CREATE TABLE`. |
| `watermark_delay` | none | none | Event-time delay (`5 minutes`). Required for windowed `CREATE TABLE`. |
| `compute` | none | none | Declared compute: named `{name, type: sql\|sdl, value}` items, or `type: py` `{path, handler}`. See [Declared compute](#declared-compute). |

A key in `skippr.yml` beats its environment fallback. Flags accept `true`/`false`, `yes`/`no`, or `1`/`0`.

## Split records into tables

Set `namespace_fields` when one source carries several kinds of record. With `namespace_fields: event_type`, records with `event_type: signup` and `event_type: purchase` land in separate tables, each with its own schema. Use several fields, such as `category,sub_category`, to split on their combination.

Without `namespace_fields`, all records from a source share one table, apart from sources that already produce one table per object, such as database tables.

## Partition output

Partitions organise files into folders so query engines can skip folders a filter rules out.

- **By field:** `batch_partition_fields: country` writes `p_country=GB/`, `p_country=US/`, and so on. To cap the number of partitions, list the values you want in `partition_allowed_values`; other values are not used as partition values.
- **By time:** `batch_time_fields: created_at` with `batch_time_unit: day` writes `year=2026/month=10/day=9/` folders on file sinks (S3, GCS, Azure Blob, Local file, Hive Athena). Each unit includes the coarser ones above it. `time_partition_prefix: p_` changes those to `p_year=2026/p_month=10/p_day=9/`. Iceberg destinations (SkipprLake, Athena Iceberg, DuckDB Iceberg) use one hidden `year|month|day|hour` transform on that timestamp instead, and identity-partition `batch_partition_fields`. `minute` is File-only.

Setting `batch_time_unit` without `batch_time_fields`, or `partition_allowed_values` without `batch_partition_fields`, stops the run with `Config dependency missing: ...`.

Partition folders apply to destinations that write files, such as S3, GCS, Azure Blob, Local file, and Athena. Warehouse destinations load tables directly; see each connector page.

## Flatten nested objects

With `flatten_events: true`, a record like:

```json
{ "id": 7, "contact": { "name": "Ada", "email": "ada@example.com" } }
```

lands with columns `id`, `contact_name`, and `contact_email` instead of an `id` column and a nested `contact` struct. Flatten when your destination or BI tool handles flat columns better than nested ones.

Flattening does not type-split mixed values inside arrays. An array of objects still has one shared element type. If a later element’s leaf cannot coerce to that type, the row is deadlettered as `ARRAY_MIXED_TYPES`. See [Schema discovery and evolution](/concepts/schema).

## Read records nested in an array

Some APIs and exports wrap records in an envelope:

```json
{ "page": 1, "data": [ { "id": 1 }, { "id": 2 } ] }
```

Set `record_field_path` to the array so each element becomes a record. Without it, Skipprd treats the whole object as one record.

## Add fixed fields

`inject_fields` adds the same fields to every record after `type: py` and before `flatten_events`, for example to tag where data came from. `type: copy` cannot set `inject_fields`.

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

## Declared compute

`compute` is a list of named items. SQL and SDL items are `{name, type, value}`. `type: py` is `{name, type, path, handler}`: `path` is a `.py` file relative to `skippr.yml`, `handler` is the function name in that file. SQL is `CREATE VIEW` / `CREATE TABLE AS SELECT` (GROUP BY, `date_trunc`, `DISTINCT ON`) or ingest `SELECT` filter/derive. Skipprd loads the Python file in a bounded `python3` worker on decoded JSON objects (after array unwrap), before `flatten_events` and before SQL. The worker puts that file's directory on `sys.path`, so sibling imports (`from lookup import lookup`) resolve. `lookup()` is user code, not an engine builtin. Inline `value:` on `type: py` is an unknown field. Skipprd binds all three lanes to `ComputeKind` and journals them in Skippr Store, keyed by `name`. Editing an existing SQL/SDL name without `ALTER COMPUTE` fails closed. `skipprd query` and `skipprd serve` register `pipeline.compute_name`. View / keyed / windowed accept [`ALTER TABLE`](/operations/alter) sugar; Latest and stream-table joins are `DROP` then `CREATE`. `type: py` digest mismatch is `DROP COMPUTE <name>`; the next sync admits the files. Stateless requires `ALTER COMPUTE`. Ad-hoc `CREATE` / `DROP` in `skipprd query` is supported; Git `transform.compute` is the usual way to ship.

```yaml
transform:
  event_time: created_at
  watermark_delay: 5 minutes
  compute:
    - name: invoices.customer_totals
      type: sql
      value: |
        CREATE TABLE invoices.customer_totals AS
        SELECT customer_id, SUM(amount) AS invoice_total, COUNT(*) AS invoice_count
        FROM invoices.orders
        GROUP BY customer_id
    - name: enrich
      type: py
      path: transforms/enrich.py
      handler: transform
```

```python
# transforms/lookup.py
def lookup(device_id):
    return CACHE.get(device_id)

# transforms/enrich.py
from lookup import lookup

def transform(record):
    detail = record.setdefault("detail", {})
    site = lookup(record.get("device_id"))
    if site:
        detail["site"] = site
    return record
```

`type: copy` may set `type: py` only (`sql` and `sdl` fail closed). Python runs on the raw JSON object; the copy identity is the transformed compact JSON line. Copy still does not flatten, inject, sort, or infer a schema. A handler error deadletters that record as `PYTHON_COMPUTE`. Changing `path`, `handler`, or any sibling `.py` in that directory changes the journal hash. Digest mismatch is `DROP COMPUTE <name>`; the next sync admits the files. WAL, offsets, and the destination stay. Ingest deadletters those batches as `PYTHON_COMPUTE` if disk path, handler, or digest does not match the journal. Absolute `path`, `..`, or a missing file fails closed. `python3` must be on `PATH`. See [Materialised views](/concepts/materialised-views).

## Next steps

- [Materialised views](/concepts/materialised-views)
- [Stream SQL](/concepts/stream-sql)
- [Skippr Store](/configuration/skippr-store)
- [Destinations](/configuration/output)
- [Schema discovery and evolution](/concepts/schema)
- [Pipelines](/configuration/pipeline)
