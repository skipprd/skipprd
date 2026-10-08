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

data_sinks:
  landing:
    schema_sink: schema_sinks.catalog
    Athena:
      s3_bucket: my-warehouse-bucket
      s3_prefix: bronze/events
      glue_database_name: bronze_events
      athena_workgroup_name: primary
      athena_results_s3_bucket: athena-results

schema_sinks:
  catalog:
    Glue:
      s3_bucket: my-warehouse-bucket
      s3_prefix: bronze/events
      athena_workgroup_name: primary
      athena_results_s3_bucket: athena-results
      glue_database_name: bronze_events
```

| Field | Required | Description |
| --- | --- | --- |
| `glue_database_name` | Yes | Glue database for table and partition DDL |
| `s3_bucket` | Yes | Bucket the Athena sink lands data in; table locations point here |
| `s3_prefix` | Yes | Prefix under `s3_bucket` for those table locations |
| `athena_workgroup_name` | Yes | Athena workgroup used for catalog DDL |
| `athena_results_s3_bucket` | Yes | Bucket for Athena query results |

Environment variable equivalent: `SCHEMA_OUTPUT_GLUE_DATABASE_NAME`.

## Pipeline wiring

1. Set `pipelines.<name>.data_sink` to a registry entry using the `Athena` plugin.
2. Set `data_sinks.<name>.schema_sink` on that sink to a registry entry using the `Glue` plugin.
3. Use the same database name, bucket, prefix, and workgroup on both blocks unless you intentionally separate landing and catalog namespaces.

## Query and model

Ingest uses `data_sinks` / `schema_sinks`. The `Athena:` sink fields (`glue_database_name`, `athena_workgroup_name`, `athena_results_s3_bucket`) are the catalog and workgroup names. There is no separate warehouse block.

## Related

- [Athena output](../outputs/athena.md) — Parquet layout and S3 paths
- [Output destination](../../configuration/output.md) — generic `data_sinks` / `schema_sinks` shape
- [Source landing semantics](../../concepts/source-landing-semantics.md) — partition and write-policy behavior
