---
title: Connector catalog
description: Every Skipprd source, destination, and schema sink — pick a connector and open its setup guide.
---

# Connectors

A pipeline reads from one **source** and writes to one **destination**. Some destinations also need a **schema sink** so the warehouse catalog stays in step with the tables Skipprd creates.

Configure them in `skippr.yml`, Python, or `skipprd connect`. Pin a release with `version:` on that entry when you do not want “latest”.

## Data sources

### Databases

| Connector | Guide |
|---|---|
| `Clickhouse` | [ClickHouse](/connectors/inputs/clickhouse) |
| `Dynamodb` | [DynamoDB](/connectors/inputs/dynamodb) |
| `Mongodb` | [MongoDB](/connectors/inputs/mongodb) |
| `Motherduck` | [MotherDuck](/connectors/inputs/motherduck) |
| `Mssql` | [SQL Server](/connectors/inputs/mssql) |
| `Mysql` | [MySQL](/connectors/inputs/mysql) |
| `Postgres` | [PostgreSQL](/connectors/inputs/postgres) |
| `Redshift` | [Redshift](/connectors/inputs/redshift) |
| `DeltaLake` | [Delta Lake](/connectors/inputs/delta_lake) |

### Files and object stores

| Connector | Guide |
|---|---|
| `File` | [Local file](/connectors/inputs/file) |
| `S3` | [S3](/connectors/inputs/s3) |
| `Sftp` | [SFTP](/connectors/inputs/sftp) |

### Streaming

| Connector | Guide |
|---|---|
| `Kafka` | [Kafka](/connectors/inputs/kafka) |
| `Sqs` | [SQS](/connectors/inputs/sqs) |
| `Kinesis` | [Kinesis](/connectors/inputs/kinesis) |
| `Amqp` | [AMQP](/connectors/inputs/amqp) |
| `Sns` | [SNS](/connectors/inputs/sns) |
| `Eventbridge` | [EventBridge](/connectors/inputs/eventbridge) |
| `Mqtt` | [MQTT](/connectors/inputs/mqtt) |
| `Websocket` | [WebSocket](/connectors/inputs/websocket) |

### HTTP and network

| Connector | Guide |
|---|---|
| `HttpClient` | [HTTP client](/connectors/inputs/http_client) |
| `HttpServer` | [HTTP server](/connectors/inputs/http_server) |
| `Socket` | [Socket](/connectors/inputs/socket) |
| `Statsd` | [StatsD](/connectors/inputs/statsd) |
| `Pcap` | [PCAP](/connectors/inputs/pcap) |
| `Otlp` | [OTLP](/connectors/inputs/otlp) |
| `Stdin` | [Stdin](/connectors/inputs/stdin) |

### Marketing, commerce, and CRM

| Connector | Guide |
|---|---|
| `GoogleAnalytics` | [Google Analytics (GA4)](/connectors/inputs/google_analytics) |
| `GoogleAds` | [Google Ads](/connectors/inputs/google_ads) |
| `GoogleSearchConsole` | [Google Search Console](/connectors/inputs/google_search_console) |
| `BingWebmasterTools` | [Bing Webmaster Tools](/connectors/inputs/bing_webmaster_tools) |
| `AppleSearchAds` | [Apple Search Ads](/connectors/inputs/apple_search_ads) |
| `MetaAds` | [Meta Ads](/connectors/inputs/meta_ads) |
| `MetaInstagramAds` | [Meta Instagram Ads](/connectors/inputs/meta_instagram_ads) |
| `LinkedInAds` | [LinkedIn Ads](/connectors/inputs/linkedin_ads) |
| `XAds` | [X Ads](/connectors/inputs/x_ads) |
| `AdrollAds` | [AdRoll Ads](/connectors/inputs/adroll_ads) |
| `Stripe` | [Stripe](/connectors/inputs/stripe) |
| `ShopifyAdmin` | [Shopify Admin](/connectors/inputs/shopify_admin) |
| `HubspotCrm` | [HubSpot CRM](/connectors/inputs/hubspot_crm) |
| `XeroAccounting` | [Xero Accounting](/connectors/inputs/xero_accounting) |
| `RevolutBusiness` | [Revolut Business](/connectors/inputs/revolut_business) |
| `SumUp` | [SumUp](/connectors/inputs/sumup) |

## Destinations

| Connector | Guide |
|---|---|
| `Athena` | [Athena](/connectors/outputs/athena) |
| `AthenaIceberg` | [Athena Iceberg](/connectors/outputs/athenaiceberg) |
| `Bigquery` | [BigQuery](/connectors/outputs/bigquery) |
| `Clickhouse` | [ClickHouse](/connectors/outputs/clickhouse) |
| `Databricks` | [Databricks](/connectors/outputs/databricks) |
| `Duckdb` | [DuckDB](/connectors/outputs/duckdb) |
| `Motherduck` | [MotherDuck](/connectors/outputs/motherduck) |
| `Postgres` | [PostgreSQL](/connectors/outputs/postgres) |
| `Redshift` | [Redshift](/connectors/outputs/redshift) |
| `Snowflake` | [Snowflake](/connectors/outputs/snowflake) |
| `Synapse` | [Synapse](/connectors/outputs/synapse) |
| `SkipprLake` | [SkipprLake](/connectors/outputs/skipprlake) |
| `S3` | [S3](/connectors/outputs/s3) |
| `Gcs` | [GCS](/connectors/outputs/gcs) |
| `AzureBlob` | [Azure Blob](/connectors/outputs/azure_blob) |
| `Sftp` | [SFTP](/connectors/outputs/sftp) |
| `File` | [Local file](/connectors/outputs/file) |
| `Amqp` | [AMQP](/connectors/outputs/amqp) |
| `Stdout` | [Stdout](/connectors/outputs/stdout) |

## Schema sinks

Pair these with the matching destination so tables and columns appear in the catalog.

| Connector | Guide | Pair with |
|---|---|---|
| `Glue` | [Glue](/connectors/schema_sinks/glue) | Athena |
| `AthenaIceberg` | [Athena Iceberg schema](/connectors/schema_sinks/athenaiceberg) | Athena Iceberg |
| `Duckdb` | [DuckDB schema](/connectors/schema_sinks/duckdb) | DuckDB |
| `SkipprLake` | [SkipprLake schema](/connectors/schema_sinks/skipprlake) | SkipprLake |

## Next steps

- [Sources](/configuration/input)
- [Destinations](/configuration/output)
- [skipprd connect](/cli/connect)
