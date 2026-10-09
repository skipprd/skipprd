---
title: Deadletters
description: Keep records that fail validation out of the main table, inspect them, and decide whether to fix the source or the schema.
---

# Deadletters

When a record fails validation or cannot be normalised, Skipprd does not write it into the main table. It becomes a **deadletter**: the original payload plus the error.

You choose what happens next.

- Point the pipeline at a `deadletter_sink` and Skipprd writes those records to that destination.
- Leave `deadletter_sink` unset and Skipprd counts and logs them, then discards them.
- If `deadletter_sink` points at a missing or invalid sink, startup fails.

Do not reuse the primary `data_sink` as the deadletter sink. Use a separate prefix, schema, or database so failed rows cannot mix with good ones.

## Configure

This pipeline writes good rows to Athena and failed rows to a second Athena database:

::: code-group

```yaml [YAML]
pipelines:
  bike_hire:
    data_source: data_sources.source
    data_sink: data_sinks.analytics
    deadletter_sink: deadletter_sinks.analytics_deadletters

data_sinks:
  analytics:
    Athena:
      athena_workgroup_name: analytics
      s3_bucket: my-main-bucket
      athena_results_s3_bucket: my-query-results
      s3_prefix: warehouse/events
    schema_sink: schema_sinks.glue_analytics

deadletter_sinks:
  analytics_deadletters:
    Athena:
      athena_workgroup_name: analytics
      s3_bucket: my-deadletter-bucket
      athena_results_s3_bucket: my-query-results
      s3_prefix: warehouse/deadletters
    schema_sink: schema_sinks.glue_analytics_deadletters

schema_sinks:
  glue_analytics:
    Glue:
      s3_bucket: my-main-bucket
      s3_prefix: warehouse/events
      athena_workgroup_name: analytics
      athena_results_s3_bucket: my-query-results
      glue_database_name: analytics
  glue_analytics_deadletters:
    Glue:
      s3_bucket: my-deadletter-bucket
      s3_prefix: warehouse/deadletters
      athena_workgroup_name: analytics
      athena_results_s3_bucket: my-query-results
      glue_database_name: analytics_deadletters
```

```python [Python]
from skippr import Config, DataSinkAthena, Pipeline, SchemaSinkGlue

cfg = Config.discover()
glue = cfg.schema_sink(
    "glue_analytics",
    SchemaSinkGlue(
        s3_bucket="my-main-bucket",
        s3_prefix="warehouse/events",
        athena_workgroup_name="analytics",
        athena_results_s3_bucket="my-query-results",
        glue_database_name="analytics",
    ),
)
glue_dl = cfg.schema_sink(
    "glue_analytics_deadletters",
    SchemaSinkGlue(
        s3_bucket="my-deadletter-bucket",
        s3_prefix="warehouse/deadletters",
        athena_workgroup_name="analytics",
        athena_results_s3_bucket="my-query-results",
        glue_database_name="analytics_deadletters",
    ),
)
warehouse = cfg.data_sink(
    "analytics",
    DataSinkAthena(
        athena_workgroup_name="analytics",
        s3_bucket="my-main-bucket",
        athena_results_s3_bucket="my-query-results",
        s3_prefix="warehouse/events",
    ),
    schema_sink=glue,
)
dl = cfg.deadletter_sink(
    "analytics_deadletters",
    DataSinkAthena(
        athena_workgroup_name="analytics",
        s3_bucket="my-deadletter-bucket",
        athena_results_s3_bucket="my-query-results",
        s3_prefix="warehouse/deadletters",
    ),
    schema_sink=glue_dl,
)
cfg.pipeline(
    "bike_hire",
    Pipeline(
        data_source=cfg.get_data_source("source"),
        data_sink=warehouse,
        deadletter_sink=dl,
    ),
)
cfg.save()
```

```bash [CLI]
skipprd connect data-sink athena \
  --pipeline bike_hire \
  --name analytics \
  --athena-workgroup-name analytics \
  --s3-bucket my-main-bucket \
  --athena-results-s3-bucket my-query-results \
  --s3-prefix warehouse/events

skipprd connect schema-sink glue \
  --pipeline bike_hire \
  --name glue_analytics \
  --s3-bucket my-main-bucket \
  --s3-prefix warehouse/events \
  --athena-workgroup-name analytics \
  --athena-results-s3-bucket my-query-results \
  --glue-database-name analytics
```

`skipprd connect` does not write `deadletter_sinks`. Add that block in YAML or with `Config.deadletter_sink` as in the Python tab.

:::

`S3` and `File` destinations also work as deadletter sinks.

## What a deadletter row contains

| Column | Description |
|---|---|
| `id` | Stable id from the source namespace and offset |
| `namespace` | Source table that failed |
| `record` | Original payload |
| `error` | Human-readable failure |
| `failure_code` | Error class |
| `event_time` | Source event time when the source provided one |
| `processed_time` | When Skipprd emitted the deadletter |
| `source_uri` | Source file or object path |
| `offset_key` / `offset_pos` | Resume coordinates of the failed record |

Athena deadletter tables are named `_dl_<pipeline>` in the deadletter database:

```sql
SELECT id, namespace, error, failure_code
FROM _dl_bike_hire
WHERE namespace = 'rides'
ORDER BY processed_time DESC
LIMIT 50;
```

## What to do with them

1. Read the `error` and `failure_code`. Most failures are a type mismatch or a required field that is missing.
2. Fix the source payload, or [review the schema](/concepts/schema) if Skipprd inferred the wrong type.
3. Re-run sync. Skipprd does not automatically replay deadletters into the main table.

## Next steps

- [Schema discovery and evolution](/concepts/schema)
- [Troubleshooting](/operations/troubleshooting)
- [skipprd sync](/cli/sync)
