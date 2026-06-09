# Warehouses

`warehouses` configure query, modeling, catalog, and dbt behavior for the `skippr` product CLI.

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

`pipelines.<name>.model.warehouse` selects a warehouse for that pipeline. If it is omitted, Skippr uses `skippr.default_warehouse`.

## Athena

```yaml
warehouses:
  primary:
    kind: athena
    workgroup: primary
    region: us-east-1
    catalog: AwsDataCatalog
    schema: analytics
    result_s3: s3://athena-query-results/
```

## Snowflake

```yaml
warehouses:
  primary:
    kind: snowflake
    account: my-org-my-account
    user: ${SNOWFLAKE_USER}
    private_key_path: ${SNOWFLAKE_PRIVATE_KEY_PATH}
    database: ANALYTICS
    schema: RAW
    warehouse: COMPUTE_WH
    role: TRANSFORMER
```

## Postgres

```yaml
warehouses:
  primary:
    kind: postgres
    database: analytics
    schema: public
```

Connection secrets can come from environment variables such as `POSTGRES_HOST`, `POSTGRES_USER`, and `POSTGRES_PASSWORD`.

## Other providers

Skippr warehouse providers also cover BigQuery, Databricks, Redshift, ClickHouse, MotherDuck, Synapse, and MSSQL where the data-engineering suite supports those capabilities.

Use output connector pages for ingest/write configuration. Use this page for query/model/catalog configuration.
