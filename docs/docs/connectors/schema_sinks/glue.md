# Glue Schema Sink

Manages AWS Glue Data Catalog databases and tables for pipelines that land Parquet on S3 through the [Athena data sink](../outputs/athena.md).

Use a schema sink when catalog DDL should be configured separately from the data sink, or when multiple pipelines share one Glue database.

## When to use

- **Athena ingest** — pair `data_sinks` → `Athena` with `schema_sinks` → `Glue` using the same `glue_database_name`.
- **Partition semantics** — Glue partition keys follow discovered schema and [source landing semantics](../../concepts/source-landing-semantics.md) when the source declares `replace_partition`.

Bundled Athena ingest can create Glue objects on write; a dedicated schema sink keeps catalog configuration explicit in `skippr.yml`.

## Configuration

```yaml
pipelines:
  events:
    data_sink: data_sinks.landing
    schema_sink: schema_sinks.catalog

data_sinks:
  landing:
    Athena:
      s3_bucket: my-warehouse-bucket
      s3_prefix: bronze/events
      glue_database_name: bronze_events

schema_sinks:
  catalog:
    Glue:
      glue_database_name: bronze_events
```

| Field | Required | Description |
| --- | --- | --- |
| `glue_database_name` | Yes | Glue database for table and partition DDL |

Environment variable equivalent: `SCHEMA_OUTPUT_GLUE_DATABASE_NAME`.

## Pipeline wiring

1. Set `pipelines.<name>.data_sink` to a registry entry using the `Athena` plugin.
2. Set `pipelines.<name>.schema_sink` to a registry entry using the `Glue` plugin.
3. Use the same database name on both blocks unless you intentionally separate landing and catalog namespaces.

## Query and model

Ingest uses `data_sinks` / `schema_sinks`. `sde query` and `sde model` use the same `Athena:` sink (`glue_database_name`, `athena_workgroup_name`, `athena_results_s3_bucket`). There is no separate warehouse block.

## Related

- [Athena output](../outputs/athena.md) — Parquet layout and S3 paths
- [Output destination](../../configuration/output.md) — generic `data_sinks` / `schema_sinks` shape
- [Source landing semantics](../../concepts/source-landing-semantics.md) — partition and write-policy behavior
