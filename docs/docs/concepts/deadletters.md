# Deadletters

Records that fail validation or cannot be normalized are captured as deadletters instead of being mixed into the primary output.

## How deadletters work

- Deadletters still go through Skippr's normal WAL and compaction pipeline.
- A pipeline can optionally point at a dedicated deadletter sink from the top-level `deadletter_sinks` registry.
- If `deadletter_sink` is unset for a pipeline, deadletter records are discarded after being counted and logged.
- If `deadletter_sink` is set but the referenced sink is invalid, startup fails.
- Deadletter Athena tables use the `_dl_<pipeline>` name, so the deadletter schema stays isolated from the primary table even when both live in Athena.

## Config file example

```yaml
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
      glue_database_name: analytics
  glue_analytics_deadletters:
    Glue:
      glue_database_name: analytics_deadletters
```

You can also use `S3` or `File` sinks in `deadletter_sinks`.

## Deadletter schema

Deadletter tables are written as Parquet with these columns:

| Column | Type | Description |
|---|---|---|
| `id` | string | Stable deadletter identifier derived from namespace and offset |
| `namespace` | string | Source namespace that failed |
| `record` | string | Original raw record payload |
| `error` | string | Human-readable failure message |
| `failure_code` | string | Error classification |
| `event_time` | bigint | Source event time when available |
| `processed_time` | bigint | Time Skippr emitted the deadletter |
| `source_uri` | string | Source file or object path |
| `offset_key` | string | Source offset key |
| `offset_pos` | bigint | Source offset position |

## Querying Athena deadletters

When the deadletter sink is Athena, query the configured deadletter database using `_dl_<pipeline>` as the table name:

```sql
SELECT id, namespace, error, failure_code
FROM _dl_bike_hire
WHERE namespace = 'rides'
ORDER BY processed_time DESC
LIMIT 50;
```

## Operational guidance

- Prefer a dedicated Glue database or S3 prefix for deadletters.
- Do not point `deadletter_sink` at the same registry entry as the primary `data_sink`.
