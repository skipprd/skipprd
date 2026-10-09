---
title: Glue schema sink
description: Create and update Glue databases and tables so Athena can query what Skipprd writes.
---

# Glue

Pair this schema sink with the [Athena](/connectors/outputs/athena) destination. Skipprd creates the Glue database and tables, and keeps columns in step with discover. You do not run DDL in the AWS console.

## Before you begin

- The same S3 prefix, Glue database, workgroup, and results bucket as the Athena destination.
- IAM for Glue `CreateDatabase`, `CreateTable`, `UpdateTable`, and partition APIs.

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkAthena, Pipeline, SchemaSinkGlue

cfg = Config.discover()
catalog = cfg.schema_sink(
    "catalog",
    SchemaSinkGlue(
        s3_bucket="my-output-bucket",
        s3_prefix="warehouse/events",
        glue_database_name="my_database",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="my-athena-results",
        region="us-east-1",
    ),
)
warehouse = cfg.data_sink(
    "warehouse",
    DataSinkAthena(
        s3_bucket="my-output-bucket",
        s3_prefix="warehouse/events",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="my-athena-results",
    ),
    schema_sink=catalog,
)
cfg.pipeline("events", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=warehouse))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink athena \
  --pipeline events \
  --name warehouse \
  --s3-bucket my-output-bucket \
  --s3-prefix warehouse/events \
  --athena-workgroup-name primary \
  --athena-results-s3-bucket my-athena-results

skipprd connect schema-sink glue \
  --pipeline events \
  --name catalog \
  --s3-bucket my-output-bucket \
  --s3-prefix warehouse/events \
  --glue-database-name my_database \
  --athena-workgroup-name primary \
  --athena-results-s3-bucket my-athena-results \
  --region us-east-1
```

```yaml [YAML]
schema_sinks:
  catalog:
    Glue:
      s3_bucket: my-output-bucket
      s3_prefix: warehouse/events
      glue_database_name: my_database
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
      region: us-east-1

data_sinks:
  warehouse:
    Athena:
      s3_bucket: my-output-bucket
      s3_prefix: warehouse/events
      athena_workgroup_name: primary
      athena_results_s3_bucket: my-athena-results
    schema_sink: schema_sinks.catalog
```

:::

Set `schema_sink` on the data sink, not on the pipeline.

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `s3_bucket` | string | Required | Same bucket as the Athena destination |
| `s3_prefix` | string | Required | Same prefix as the Athena destination |
| `athena_workgroup_name` | string | Required | Athena workgroup |
| `athena_results_s3_bucket` | string | Required | Results bucket name |
| `glue_database_name` | string | Required | Glue database |
| `format` | string | Not set | Table format override |
| `region` | string | Not set | AWS region |
| `catalog` | string | Not set | Glue catalog name |
| `max_concurrency` | integer | Not set | Query/model only |
| `discovery_cache_ttl_secs` | integer | Not set | Query/model only |

## Troubleshooting

| Symptom | Fix |
|---|---|
| `TABLE_NOT_FOUND` in Athena | Confirm this sink is referenced from the data sink and discover has run |
| Glue `AccessDenied` | Grant database and table APIs |
| Partition missing | Re-run sync — partitions register as files land |

## Next steps

- [Athena](/connectors/outputs/athena)
- [Quickstart: S3 to Athena](/getting-started/quickstart)
