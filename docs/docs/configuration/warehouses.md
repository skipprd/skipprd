# Warehouses

`warehouses` configure query, modeling, catalog, and dbt behavior for the `skippr` CLI.

They do **not** replace `data_sinks`.

- `data_sinks` write ingested data.
- `warehouses` query and model data after it lands.

## Basic shape

```yaml
skippr:
  default_warehouse: primary

pipelines:
  google_analytics:
    data_source: data_sources.ga4
    data_sink: data_sinks.athena
    model:
      warehouse: primary

warehouses:
  primary:
    kind: athena
    workgroup: primary
    catalog: AwsDataCatalog
    schema: analytics
    result_s3: s3://athena-query-results/
```

`pipelines.<name>.model.warehouse` selects a warehouse for that pipeline. If omitted, Skippr uses `skippr.default_warehouse`.

## Provider reference

| `kind` | Doc |
| --- | --- |
| `athena` | [Athena](../connectors/warehouses/athena.md) |
| `snowflake` | [Snowflake](../connectors/warehouses/snowflake.md) |
| `postgres` | [Postgres](../connectors/warehouses/postgres.md) |
| `bigquery` | [BigQuery](../connectors/warehouses/bigquery.md) |
| `databricks` | [Databricks](../connectors/warehouses/databricks.md) |
| `redshift` | [Redshift](../connectors/warehouses/redshift.md) |
| `clickhouse` | [ClickHouse](../connectors/warehouses/clickhouse.md) |
| `motherduck` | [MotherDuck](../connectors/warehouses/motherduck.md) |
| `synapse` | [Synapse](../connectors/warehouses/synapse.md) |
| `mssql` | [MSSQL](../connectors/warehouses/mssql.md) |

Use [data sink](../connectors/index.md#data-sinks) connector pages for ingest configuration. Use the warehouse pages above for query and modeling.

See the [connector index](../connectors/index.md) for the full list of sources, sinks, and schema sinks.
