# Connectors

Reference for configuring data sources, ingest sinks, schema sinks, and query warehouses in `skippr.yml`.

| Role | YAML block | Overview |
| --- | --- | --- |
| Read / extract | `data_sources:` | Input connectors (table below) |
| Write ingested data | `data_sinks:` | [Data sinks](#data-sinks) |
| Catalog / DDL for ingest | `schema_sinks:` | [Schema sinks](#schema-sinks) |
| Query, model, dbt | `warehouses:` | [Warehouse providers](#warehouse-providers) |

`data_sinks` and `schema_sinks` configure **ingest**. `warehouses` configure **query and modeling** after data lands. A destination often needs both an ingest sink and a warehouse block.

Optional `version:` on a source, sink, or schema connector pins that connector to a specific release.

## Data sources

| Plugin | Doc | `skippr connect` |
| --- | --- | --- |
| `Clickhouse` | [ClickHouse](inputs/clickhouse.md) | — |
| `Dynamodb` | [DynamoDB](inputs/dynamodb.md) | — |
| `Mongodb` | [MongoDB](inputs/mongodb.md) | — |
| `Motherduck` | [MotherDuck](inputs/motherduck.md) | — |
| `Mssql` | [MSSQL](inputs/mssql.md) | — |
| `Mysql` | [MySQL](inputs/mysql.md) | — |
| `Postgres` | [PostgreSQL](inputs/postgres.md) | — |
| `Redshift` | [Redshift](inputs/redshift.md) | — |
| `DeltaLake` | [Delta Lake](inputs/delta_lake.md) | — |
| `File` | [Local file](inputs/file.md) | — |
| `S3` | [S3](inputs/s3.md) | — |
| `Sftp` | [SFTP](inputs/sftp.md) | — |
| `Kafka` | [Kafka](inputs/kafka.md) | — |
| `Sqs` | [SQS](inputs/sqs.md) | — |
| `Kinesis` | [Kinesis](inputs/kinesis.md) | — |
| `Amqp` | [AMQP](inputs/amqp.md) | — |
| `Sns` | [SNS](inputs/sns.md) | — |
| `Eventbridge` | [EventBridge](inputs/eventbridge.md) | — |
| `Mqtt` | [MQTT](inputs/mqtt.md) | — |
| `Websocket` | [WebSocket](inputs/websocket.md) | — |
| `GoogleAnalytics` | [GA4](inputs/google_analytics.md) | `google-analytics` |
| `GoogleSearchConsole` | [Search Console](inputs/google_search_console.md) | `google-search-console` |
| `BingWebmasterTools` | [Bing Webmaster](inputs/bing_webmaster_tools.md) | `bing-webmaster-tools` |
| `AppleSearchAds` | [Apple Search Ads](inputs/apple_search_ads.md) | — |
| `HttpClient` | [HTTP client](inputs/http_client.md) | — |
| `HttpServer` | [HTTP server](inputs/http_server.md) | — |
| `GoogleAds` | [Google Ads](inputs/google_ads.md) | — |
| `MetaAds` | [Meta Ads](inputs/meta_ads.md) | — |
| `MetaInstagramAds` | [Meta Instagram Ads](inputs/meta_instagram_ads.md) | `meta-instagram-ads` |
| `LinkedInAds` | [LinkedIn Ads](inputs/linkedin_ads.md) | — |
| `XAds` | [X Ads](inputs/x_ads.md) | — |
| `AdrollAds` | [AdRoll Ads](inputs/adroll_ads.md) | — |
| `Stripe` | [Stripe](inputs/stripe.md) | — |
| `ShopifyAdmin` | [Shopify Admin](inputs/shopify_admin.md) | — |
| `HubspotCrm` | [HubSpot CRM](inputs/hubspot_crm.md) | — |
| `XeroAccounting` | [Xero Accounting](inputs/xero_accounting.md) | — |
| `RevolutBusiness` | [Revolut Business](inputs/revolut_business.md) | — |
| `SumUp` | [SumUp](inputs/sumup.md) | — |
| `Socket` | [Socket](inputs/socket.md) | — |
| `Statsd` | [StatsD](inputs/statsd.md) | — |
| `Stdin` | [Stdin](inputs/stdin.md) | — |
| `Pcap` | [PCAP](inputs/pcap.md) | — |

## Data sinks

| Plugin | Doc |
| --- | --- |
| `Athena` | [Athena (S3 + Glue)](outputs/athena.md) |
| `Bigquery` | [BigQuery](outputs/bigquery.md) |
| `Clickhouse` | [ClickHouse](outputs/clickhouse.md) |
| `Databricks` | [Databricks](outputs/databricks.md) |
| `Motherduck` | [MotherDuck](outputs/motherduck.md) |
| `Postgres` | [Postgres](outputs/postgres.md) |
| `Redshift` | [Redshift](outputs/redshift.md) |
| `Snowflake` | [Snowflake](outputs/snowflake.md) |
| `Synapse` | [Synapse](outputs/synapse.md) |
| `Iceberg` | [Iceberg](outputs/iceberg.md) |
| `S3` | [S3](outputs/s3.md) |
| `Gcs` | [GCS](outputs/gcs.md) |
| `AzureBlob` | [Azure Blob](outputs/azure_blob.md) |
| `Sftp` | [SFTP](outputs/sftp.md) |
| `File` | [Local file](outputs/file.md) |
| `Amqp` | [AMQP](outputs/amqp.md) |
| `Stdout` | [Stdout](outputs/stdout.md) |

## Schema sinks

| Plugin | Doc | Typical data sink |
| --- | --- | --- |
| `Glue` | [Glue](schema_sinks/glue.md) | `Athena` |
| `Iceberg` | [Iceberg](schema_sinks/iceberg.md) | `Iceberg` |
| `Bigquery` | — | `Bigquery` |
| `Snowflake` | — | `Snowflake` |
| `Postgres` | — | `Postgres` |
| `Redshift` | — | `Redshift` |
| `Clickhouse` | — | `Clickhouse` |
| `Motherduck` | — | `Motherduck` |

## Warehouse providers

| `kind` | Doc |
| --- | --- |
| `athena` | [Athena](warehouses/athena.md) |
| `snowflake` | [Snowflake](warehouses/snowflake.md) |
| `postgres` | [Postgres](warehouses/postgres.md) |
| `bigquery` | [BigQuery](warehouses/bigquery.md) |
| `databricks` | [Databricks](warehouses/databricks.md) |
| `redshift` | [Redshift](warehouses/redshift.md) |
| `clickhouse` | [ClickHouse](warehouses/clickhouse.md) |
| `motherduck` | [MotherDuck](warehouses/motherduck.md) |
| `synapse` | [Synapse](warehouses/synapse.md) |
| `mssql` | [MSSQL](warehouses/mssql.md) |
