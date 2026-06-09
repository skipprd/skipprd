# Athena Warehouse

Query and model data in the AWS Glue Data Catalog with Amazon Athena.

Ingest uses [`data_sinks`](../outputs/athena.md) and optional [`schema_sinks`](../schema_sinks/glue.md). This page covers the `warehouses:` block for `skippr query` and `skippr model`.

## Configuration

```yaml
skippr:
  default_warehouse: primary

pipelines:
  events:
    data_sink: data_sinks.landing
    model:
      warehouse: primary

warehouses:
  primary:
    kind: athena
    workgroup: primary
    region: us-east-1
    catalog: AwsDataCatalog
    schema: analytics
    result_s3: s3://athena-query-results/
```

| Field | Description |
| --- | --- |
| `kind` | `athena` |
| `workgroup` | Athena workgroup name |
| `region` | AWS region for Athena and Glue |
| `catalog` | Glue catalog name (default `AwsDataCatalog`) |
| `schema` | Default database/schema for modeling |
| `result_s3` | S3 URI for Athena query results |
| `max_concurrency` | Parallel query cap |
| `discovery_cache_ttl_secs` | Catalog discovery cache TTL |

## Credentials

Uses the standard AWS credential chain (environment, shared config, instance role).

## Related

- [Athena ingest](../outputs/athena.md)
- [Glue schema sink](../schema_sinks/glue.md)
- [Warehouses overview](../../configuration/warehouses.md)
