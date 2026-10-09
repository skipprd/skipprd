---
title: Sources
description: Choose a data source, add it to a pipeline in skippr.yml, keep its credentials out of the file, and check that Skipprd can read it.
---

# Sources

A source is where a pipeline reads data: a database, files in object storage, a stream, or a SaaS API. You declare sources under `data_sources:` in `skippr.yml`, give each one a name, and point a pipeline at it.

## Choose a source

| Kind | Connectors |
|---|---|
| Databases | [ClickHouse](/connectors/inputs/clickhouse), [Delta Lake](/connectors/inputs/delta_lake), [DynamoDB](/connectors/inputs/dynamodb), [MongoDB](/connectors/inputs/mongodb), [MotherDuck](/connectors/inputs/motherduck), [MySQL](/connectors/inputs/mysql), [PostgreSQL](/connectors/inputs/postgres), [Redshift](/connectors/inputs/redshift), [SQL Server](/connectors/inputs/mssql) |
| Files and object stores | [Local file](/connectors/inputs/file), [S3](/connectors/inputs/s3), [SFTP](/connectors/inputs/sftp) |
| Streaming | [AMQP](/connectors/inputs/amqp), [EventBridge](/connectors/inputs/eventbridge), [Kafka](/connectors/inputs/kafka), [Kinesis](/connectors/inputs/kinesis), [MQTT](/connectors/inputs/mqtt), [SNS](/connectors/inputs/sns), [SQS](/connectors/inputs/sqs), [WebSocket](/connectors/inputs/websocket) |
| HTTP and network | [HTTP client](/connectors/inputs/http_client), [HTTP server](/connectors/inputs/http_server), [OTLP](/connectors/inputs/otlp), [PCAP](/connectors/inputs/pcap), [Socket](/connectors/inputs/socket), [StatsD](/connectors/inputs/statsd), [Stdin](/connectors/inputs/stdin) |
| Marketing and ads | [AdRoll Ads](/connectors/inputs/adroll_ads), [Apple Search Ads](/connectors/inputs/apple_search_ads), [Bing Webmaster Tools](/connectors/inputs/bing_webmaster_tools), [Google Ads](/connectors/inputs/google_ads), [Google Analytics (GA4)](/connectors/inputs/google_analytics), [Google Search Console](/connectors/inputs/google_search_console), [LinkedIn Ads](/connectors/inputs/linkedin_ads), [Meta Ads](/connectors/inputs/meta_ads), [Meta Instagram Ads](/connectors/inputs/meta_instagram_ads), [X Ads](/connectors/inputs/x_ads) |
| Commerce, finance, and CRM | [HubSpot CRM](/connectors/inputs/hubspot_crm), [Revolut Business](/connectors/inputs/revolut_business), [Shopify Admin](/connectors/inputs/shopify_admin), [Stripe](/connectors/inputs/stripe), [SumUp](/connectors/inputs/sumup), [Xero Accounting](/connectors/inputs/xero_accounting) |

Each connector page lists the fields you can set, what Skipprd reads, and how to troubleshoot it.

## Add a source to a pipeline

This example reads two Postgres tables into a pipeline called `orders`.

1. Write the source entry and the pipeline that uses it:

   ::: code-group

   ```python [Python]
   from skippr import Config, DataSourcePostgres, EnvRef, LocalStorage, Pipeline

   cfg = Config().workspace("analytics").storage(LocalStorage())
   app_db = cfg.data_source(
       "app_db",
       DataSourcePostgres(
           host="db.internal",
           user="skippr_reader",
           password=EnvRef("APP_DB_PASSWORD"),
           database="app",
           tables=["orders", "customers"],
       ),
   )
   cfg.pipeline("orders", Pipeline(data_source=app_db))
   cfg.save("skippr.yml")
   ```

   ```bash [CLI]
   skipprd --workspace analytics --storage-mode local connect data-source postgres \
     --pipeline orders \
     --name app_db \
     --host db.internal \
     --user skippr_reader \
     --password '${APP_DB_PASSWORD}' \
     --database app \
     --tables orders \
     --tables customers
   ```

   ```yaml [YAML]
   skippr:
     workspace: analytics
     skipprd_el_storage_mode: local

   pipelines:
     orders:
       data_source: data_sources.app_db

   data_sources:
     app_db:
       Postgres:
         host: db.internal
         user: skippr_reader
         password: ${APP_DB_PASSWORD}
         database: app
         tables: [orders, customers]
   ```

   :::

   `app_db` is a logical name. `Postgres` is the connector type; write it with the casing shown on the connector page. `skipprd connect` creates the `orders` pipeline if it does not exist and sets its `data_source`.

2. Provide the secret:

   ```bash
   export APP_DB_PASSWORD='your-postgres-password'
   ```

3. Discover the source's schema:

   ::: code-group

   ```python [Python]
   import skippr

   session = skippr.Session(skippr.Config.discover().get_pipeline("orders"))
   session.discover()
   ```

   ```bash [CLI]
   skipprd discover --pipeline orders
   ```

   :::

## Check it worked

Show the schema `discover` saved for the pipeline:

```bash
skipprd metadata show --pipeline orders
```

You should see the `orders` and `customers` tables with their columns. If they are missing or the command fails, see the Troubleshooting section of the connector page and [Troubleshooting](/operations/troubleshooting).

The pipeline has no destination yet, so a sync keeps the data in the write-ahead log, where `skipprd query` can read it. Add a destination next.

## Credentials

- Put every secret in `skippr.yml` as a `${NAME}` reference. Secret fields reject plaintext. The [skippr.yml reference](/configuration/skippr-yml) covers the rules and `.env` files.
- AWS sources (S3, DynamoDB, Kinesis, SQS, SNS, EventBridge) use the standard AWS credential chain: environment variables, a shared profile, or the instance or task role.
- Give Skipprd a read-only account where the source supports one.

## One source, several pipelines

Several pipelines can reference the same `data_sources` entry. Each pipeline keeps its own progress, so each reads the source independently.

## Next steps

- [Destinations](/configuration/output) — land the data in a warehouse or lake.
- [Transforms](/configuration/transforms) — split, partition, or flatten records.
- [Schema discovery and evolution](/concepts/schema) — what `discover` records and how changes are handled.
- [How sources land](/concepts/source-landing-semantics)
