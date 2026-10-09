---
title: Schema discovery and evolution
description: See how Skipprd infers types from your data, what happens when fields appear or change type, and how to split one source into many tables.
---

# Schema discovery and evolution

Skipprd builds the schema from your data. You do not write column definitions. `skipprd discover` samples the source and infers every field, including nested objects and arrays. During `skipprd sync`, Skipprd keeps evolving that schema as new fields and new types arrive, and updates your destination tables to match.

## Discover a schema

::: code-group

```python [Python]
import skippr

s = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
s.discover()
```

```bash [CLI]
skipprd discover --pipeline bikehire --log
skipprd schema --pipeline bikehire
```

:::

Discover reads a sample, infers the schema, and saves it with the pipeline. It does not load rows into your destination. `skipprd schema` prints what was saved.

Skipprd keeps the schema in the S3 bucket set by `skippr.skippr_s3_bucket`, or under `DATA_DIR` when you set `skipprd_el_storage_mode: local`. See [How Skipprd works](/concepts/how-it-works) for where state lives.

## What Skipprd detects

- **Nested objects** become structs (records) with their full depth preserved.
- **Arrays** of values and arrays of objects are kept as arrays.
- **Types** are inferred per field from the values Skipprd sees.
- **Namespaces** split one source into several tables when you set `namespace_fields` (see [Split a source into tables](#split-a-source-into-tables)).

Type inference is deterministic: the same data always produces the same schema.

## How types are inferred

| Value | Inferred type |
|---|---|
| `true` / `false` | Boolean |
| `123` | Integer |
| `9999999999` (outside the 32-bit range) | Long |
| `1.23` | Double |
| `"2024-01-15"` | Date |
| `1704067200` (Unix seconds, 2010 or later) | Timestamp (seconds) |
| `1704067200000` (Unix milliseconds, 2010 to 2040) | Timestamp (milliseconds) |
| `null` | String |
| Anything else | String |

Whole numbers that look like Unix timestamps from 2010 onwards are treated as timestamps. If a numeric ID happens to fall in that range, check `skipprd schema` after discover.

## How the schema evolves

You do not need to re-run discover when data changes. During sync:

- **A new field** is added to the schema, and Skipprd adds the column to your destination table.
- **A value that still fits the field's type** stays in the existing column. For example, a whole number or a numeric string written to a Double field lands in that field.
- **A value of a different type** does not overwrite the existing column. Skipprd adds a sibling column named after the field and the new type, and writes the value there. If `price` is a Double and the text `"n/a"` arrives, that value lands in `price_string`.
- **A value Skipprd cannot type or convert** is sent to the [deadletter](/concepts/deadletters) path. Other records in the batch continue.

Sibling columns keep every value without breaking existing queries. Watch for them in your warehouse: a new `<field>_<type>` column means a source started sending a different type.

### Fold a sibling column back

For Iceberg destinations (SkipprLake, Athena Iceberg, DuckDB), you can merge a sibling column into the original. Disable the pipeline first:

```bash
skipprd query --sql "DISABLE PIPELINE bikehire"
skipprd query --sql "ALTER TABLE bikehire.trips MERGE COLUMN price_string INTO price"
skipprd query --sql "ENABLE PIPELINE bikehire"
```

`MERGE COLUMN` drops the sibling and retargets evolution so later values write to the target. Historical src-only rows are not copied. `ALTER TABLE` also supports `RENAME COLUMN`, `DROP COLUMN`, and widening `ALTER COLUMN ... TYPE` (byte/short/integer to long, byte/short to integer, float to double, timestamp_milli to timestamp). Iceberg timestamps are already microseconds, so a milli→timestamp promote updates skippr metadata only. Nested `DROP`/`MERGE` without a top-level Iceberg field id fails closed. The pipeline must be `DISABLED` for every `ALTER TABLE`. Run `skipprd sql-help` for the full syntax.

## Flatten nested fields

Skipprd keeps nested structure by default. To write every nested field as its own dot-separated column (`contact.name`, `location.start_geo.lat`), set `flatten_events` on the pipeline's transform.

::: code-group

```python [Python]
from skippr import Config, Pipeline, Transform

cfg = Config.discover()
src = cfg.get_data_source("sample")
cfg.pipeline(
    "bikehire",
    Pipeline(data_source=src, transform=Transform(flatten_events=True)),
)
cfg.save()
```

```yaml [YAML]
pipelines:
  bikehire:
    data_source: data_sources.sample
    transform:
      flatten_events: true
```

:::

Flatten when your destination or BI tool handles flat columns better than structs. Decide before the first sync; changing it later produces different column names.

## Split a source into tables

When one source carries several record types, such as `click`, `purchase`, and `signup` events in one stream, set `namespace_fields` to the field that identifies the type. Skipprd keeps one schema and one destination table per value.

::: code-group

```python [Python]
from skippr import Config, Pipeline, Transform

cfg = Config.discover()
src = cfg.get_data_source("events")
cfg.pipeline(
    "events",
    Pipeline(data_source=src, transform=Transform(namespace_fields="event_type")),
)
cfg.save()
```

```yaml [YAML]
pipelines:
  events:
    data_source: data_sources.events
    transform:
      namespace_fields: event_type
```

:::

For a composite key, list fields separated by commas: `namespace_fields: category,sub_category`. Each unique combination becomes its own table.

Set `namespace_fields` before you run discover, so the schema is split from the start. See [Transforms](/configuration/transforms) for partitioning and ordering options.

## Next steps

- [Deadletters](/concepts/deadletters): capture records that cannot be typed or written.
- [Transforms](/configuration/transforms): partition, order, and reshape records.
- [`skipprd schema`](/cli/schema): inspect the saved schema.
- [How sources land](/concepts/source-landing-semantics): keys and write policies that sources declare.
