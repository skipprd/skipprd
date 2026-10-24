# Skipprd

[![CI](https://github.com/skipprd/skipprd/actions/workflows/ci.yml/badge.svg)](https://github.com/skipprd/skipprd/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/skipprd/skipprd)](https://github.com/skipprd/skipprd/releases)
[![Docs](https://img.shields.io/badge/docs-skippr.io-blue)](https://skippr.io)
[![License](https://img.shields.io/badge/license-PolyForm%20Shield%201.0.0-lightgrey)](./LICENSE)

Skippr (`skipprd` CLI and `import skippr` Python SDK) is an open **Streamhouse** engine in a single Rust binary. It captures continuously from OLTP databases (CDC), event streams, files, and SaaS APIs; lands atomic, ACID **Apache Iceberg** tables on Parquet in your own object store; and serves a real-time, strongly consistent SQL view over committed Iceberg snapshots and live in-stream rows.

Because storage is standard Apache Iceberg and Parquet in a bucket you control (`s3://`, Cloudflare R2, or `file:///`), **your data is never trapped**. Query live operational state in Skippr, or query committed history from Athena, Snowflake, DuckDB, Databricks, Spark, Trino, PyIceberg, and notebooks on the exact same files—with no second ETL copy and zero vendor lock-in.

**[Documentation](https://skippr.io)** · **[Install](https://skippr.io/getting-started/install)** · **[Connectors](https://skippr.io/connectors/)** · **[CLI Reference](https://skippr.io/cli/overview)** · **[Python API](https://skippr.io/python)** · **[Streamhouse Architecture](https://skippr.io/concepts/datalake)**

---

## Architecture & Guarantees

```text
                ┌────────────────────────── skipprd ──────────────────────────┐
  data source ──▶  transform & infer ──▶ commit to WAL ──▶ atomic sink commit ──▶ Iceberg / Warehouse
                │        │                     │                   │          │        │
                │        ▼                     ▼                   ▼          │        ▼
                │  in-stream schema     SkipprStore         deadletter sink   │  Athena, Snowflake,
                │  evolution & DDL   (offsets & leases)     (_dl_<pipeline>)  │  DuckDB, Spark, dbt
                └──────────────────────────────┬──────────────────────────────┘        │
                                               └───────────────┬───────────────────────┘
                                                               ▼
                                                  skipprd query / Session.query()
                                               (Committed Iceberg ∪ Live In-Flight WAL)
```

- **Zero vendor lock-in:** Pipelines land as standard Apache Iceberg tables (`SkipprLake`, `AthenaIceberg`, `Duckdb`) in your bucket, or load directly into `Snowflake`, `Bigquery`, `Postgres`, `Redshift`, `Databricks`, `Synapse`, `Clickhouse`, and `Motherduck`.
- **Dual query path:** `skipprd query` and `Session.query()` execute SQL across committed Iceberg tables unioned with uncommitted batches still in the Write-Ahead Log (WAL). External engines read ACID-committed snapshots directly or through `skipprd serve` (Iceberg REST Catalog + Arrow Flight SQL).
- **Crash-safe WAL & exactly-once delivery:** Every batch is committed to the WAL (local disk, S3, or a multi-node clustered quorum) before source offsets advance. After a crash—even `kill -9`—the next run recovers uncommitted batches from the WAL without losing or duplicating committed rows.
- **In-stream schema evolution & online DDL:** `skipprd discover` infers nested structs, arrays, and scalar types. During `skipprd sync`, new fields are added automatically and incompatible type changes route into non-breaking sibling columns (`<field>_<type>`). `ALTER TABLE` and `skipprd migrate` rename, merge, widen, or drop columns in-stream without stopping the pipeline.
- **Contract-enforced landing:** Sources declare their write policy (`append`, `merge_by_key`, `replace_partition`, or `replace_table`). If a destination cannot honor the source's contract (such as a mutable GA4 lookback window pointed at an append-only sink), Skippr fails closed at startup.
- **One engine, three interfaces:** Configure and run pipelines via `skippr.yml`, the `skipprd` CLI, or the typed Python SDK (`import skippr`). All three run the same Rust engine and merge into the same configuration file.

---

## Getting Started: From Install to Production

### 1. Install

Published builds target **macOS arm64** and **Linux x86_64**.

```bash
# CLI via Homebrew (macOS & Linux)
brew tap skipprd/tap && brew install skipprd

# CLI via install script (supports SKIPPR_INSTALL_DIR and pinned SKIPPR_VERSION=x.y.z)
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | sh

# Python SDK (Python 3.10+, ships with mypy --strict type stubs and pyarrow support)
pip install skippr "skippr[pandas]"
```

Verify the install:

```bash
skipprd --version
python -c "import skippr; print(skippr.Session)"
```

> **On-demand connectors:** Connectors are downloaded on first use from `install.skippr.io` into `~/.skippr/runtime_plugins` (override with `SKIPPR_RUNTIME_PLUGIN_DIR`). Pin a specific connector version in `skippr.yml` with `version: x.y.z` if required.

---

### 2. Configure & Ingest (Zero-Warehouse Local Start)

You do not need to provision a warehouse to ingest and query data. When a pipeline omits `data_sink`, Skippr commits batches to its local WAL and queries them directly in place.

Create a pipeline reading public sample JSON from S3 using the CLI, Python, or `skippr.yml`:

```bash
# CLI: writes skippr.yml in the current directory
skipprd --workspace quickstart --storage-mode local connect data-source s3 \
  --pipeline bikehire \
  --name sample \
  --s3-bucket skippr-public-sample-data \
  --s3-prefix bike-hire
```

```python
# Python: identical skippr.yml output and in-process Session execution
import skippr
from skippr import Config, DataSourceS3, LocalStorage, Pipeline

cfg = Config().workspace("quickstart").storage(LocalStorage())
sample = cfg.data_source(
    "sample",
    DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire"),
)
bikehire = cfg.pipeline("bikehire", Pipeline(data_source=sample))
cfg.save("skippr.yml")
```

```yaml
# skippr.yml produced by either command above
skippr:
  workspace: quickstart
  skipprd_el_storage_mode: local

pipelines:
  bikehire:
    data_source: data_sources.sample

data_sources:
  sample:
    S3:
      s3_bucket: skippr-public-sample-data
      s3_prefix: bike-hire
```

Validate the configuration, infer the schema, ingest a pass, and query the WAL with SQL:

```bash
skipprd doctor
skipprd discover --pipeline bikehire
skipprd sync --pipeline bikehire --once
skipprd query --plain --sql "SELECT count(*) AS rides FROM bikehire"
```

- **`--once` vs. continuous sync:** `skipprd sync --once` runs a single pass and exits (for cron, Airflow, Dagster, or CI). Without `--once`, `skipprd sync` runs continuously, polling every `sync_frequency_seconds` (default `900`).
- **Secrets & `.env` resolution:** Secret fields reject plaintext values. Always use `${ENV_VAR}` in YAML/CLI (`--password '${POSTGRES_PASSWORD}'`) or `EnvRef("POSTGRES_PASSWORD")` in Python. Skippr resolves variables from `.env.local` (highest precedence), the shell environment, and `.env` next to `skippr.yml`.
- **Deadletters:** Attach a `deadletter_sink` (`deadletter_sinks.<name>`) to capture records that fail validation or type normalization without halting the batch. Inspect rejected payloads and error codes via SQL:
  ```bash
  skipprd query --sql "SELECT id, namespace, failure_code, error FROM _dl_bikehire ORDER BY processed_time DESC LIMIT 20"
  ```

---

### 3. Schemas, In-Stream Evolution & Online Migrations

Skippr builds and maintains table schemas from your data automatically.

#### Inspect & Override Discovered Schemas

```bash
# Print human-readable columns and types
skipprd schema --pipeline bikehire

# Export discovered per-namespace schema metadata as JSON
skipprd metadata show --pipeline bikehire

# Override or pin a reviewed schema in CI (replaces the namespace's field list)
skipprd metadata apply --pipeline bikehire --namespace bikehire \
  --schema-json '{"fields":[{"name":"ride_id","field_type":"long","nullable":false},{"name":"station","field_type":"string"}]}'
```

Set `SCHEMA_AUTO_APPROVE=false` (or `auto_approve: false` on the pipeline) if you want schema changes gated until manually reviewed.

#### Non-Breaking Type Drift & Online `ALTER TABLE`

During `skipprd sync`:
- **New fields** are appended to the pipeline schema and destination table automatically.
- **Type conflicts** never fail the sync or drop data: if `price` was inferred as `double` and `"n/a"` arrives, Skippr creates a sibling column `price_string` and writes the value there.

On Iceberg destinations (`SkipprLake`, `AthenaIceberg`, `Duckdb`) and native-DDL warehouses (`Snowflake`, `Postgres`, `Bigquery`, `Redshift`, `Motherduck`), you can fold sibling columns back, rename fields (including into nested structs), widen types, or drop columns **in-stream without stopping the pipeline**:

```bash
# Fold a type-conflict sibling column back into the target column
skipprd query --sql "ALTER TABLE bikehire.rides MERGE COLUMN price_string INTO price"

# Move a top-level column into a nested record (preserves Iceberg field ID)
skipprd query --sql "ALTER TABLE bikehire.rides RENAME COLUMN region TO detail.region"

# Widen an integer column to 64-bit BIGINT
skipprd query --sql "ALTER TABLE bikehire.rides ALTER COLUMN trip_seconds TYPE BIGINT"

# Drop an unused column
skipprd query --sql "ALTER TABLE bikehire.rides DROP COLUMN legacy_flag"
```

To manage DDL changes as idempotent, journaled files in `migrations/*.sql`, use `skipprd migrate` or `skippr.Migration`:

```python
import skippr
from skippr import Config, Migration

Migration(
    "20261024_rename_region",
    up="ALTER TABLE bikehire.rides RENAME COLUMN region TO region_code;",
    down="ALTER TABLE bikehire.rides RENAME COLUMN region_code TO region;",
).write("migrations")

s = skippr.Session(Config.discover().get_pipeline("bikehire"))
s.migrate()
```

---

### 4. Operations & In-Stream Transforms

Configure `transform:` on any pipeline to route, reshape, partition, and sort records before they land:

```yaml
pipelines:
  events:
    data_source: data_sources.raw_stream
    data_sink: data_sinks.lake
    transform:
      record_field_path: data                       # Unwrap JSON array envelopes: {"page": 1, "data": [...]}
      namespace_fields: event_type                  # Split 1 stream into separate tables per event_type
      flatten_events: true                          # Flatten nested structs into dotted columns (user.id)
      inject_fields:                                # Stamp static metadata onto every record
        env: prod
      batch_partition_fields: country               # Hive partition folders (p_country=US/)
      partition_allowed_values: US,GB,DE            # Drop records outside allowed partition values
      batch_time_fields: created_at,ingested_at     # Timestamp column(s) for time partitioning
      batch_time_unit: day                          # Partition depth: year | month | day | hour | minute
      time_partition_prefix: p_                     # Prefix time folders (p_year=2026/p_month=10/p_day=24/)
      batch_order_fields: customer_id,created_at    # Sort rows inside Parquet files for min/max pruning

data_sources:
  raw_stream:
    S3:
      s3_bucket: acme-events
      s3_prefix: raw

data_sinks:
  lake:
    schema_sink: schema_sinks.lake_schema
    SkipprLake:
      warehouse: s3://acme-iceberg-lake/warehouse/
      catalog_table: acme-skippr-store

schema_sinks:
  lake_schema:
    SkipprLake:
      warehouse: s3://acme-iceberg-lake/warehouse/
      catalog_table: acme-skippr-store
```

Same transform via Python:

```python
from skippr import BatchTimeUnit, Config, Pipeline, Transform

cfg = Config.load("skippr.yml")
cfg.pipeline(
    "events",
    Pipeline(
        data_source=cfg.get_data_source("raw_stream"),
        data_sink=cfg.get_data_sink("lake"),
        transform=Transform(
            record_field_path="data",
            namespace_fields="event_type",
            flatten_events=True,
            inject_fields={"env": "prod"},
            batch_partition_fields="country",
            partition_allowed_values="US,GB,DE",
            batch_time_fields="created_at,ingested_at",
            batch_time_unit=BatchTimeUnit.DAY,
            time_partition_prefix="p_",
            batch_order_fields="customer_id,created_at",
        ),
    ),
)
cfg.save()
```

> **Parquet row-group pruning (`batch_order_fields`):** When sort fields are configured, Skippr sorts rows inside each Parquet file and dynamically sizes row groups between 16 MiB and 64 MiB uncompressed based on row width and leading-column cardinality—allowing query engines to skip non-matching row groups via Parquet min/max statistics.

---

### 5. Querying Real-Time Data & Serving Open Iceberg

#### Land Open Iceberg Tables (`SkipprLake`, `AthenaIceberg`, `Duckdb`)

Wire an Iceberg destination to land atomic snapshots in your object store or local disk:

```bash
skipprd connect data-sink skippr-lake \
  --pipeline bikehire \
  --name lake \
  --warehouse 's3://my-iceberg-warehouse/' \
  --catalog-table my-iceberg-catalog \
  --region us-east-1 \
  --table-namespace bronze

skipprd connect schema-sink skippr-lake \
  --pipeline bikehire \
  --name lake_schema
```

#### Real-Time SQL Over Committed Iceberg + Live WAL

For Iceberg destinations, `skipprd query` and `Session.query()` automatically union the committed Iceberg table with live rows still buffered in the WAL:

```bash
# Interactive SQL prompt
skipprd query

# Analytical SQL over <pipeline>.<namespace>
skipprd query --sql "SELECT station, count(*) AS rides FROM bikehire.rides GROUP BY station ORDER BY rides DESC LIMIT 10"

# Refresh query every 5s in the terminal while sync is running
skipprd query --sql "SELECT count(*) FROM bikehire.rides" --watch 5

# JSON output for scripts and CI assertions
skipprd query --plain --sql "SELECT count(*) AS rides FROM bikehire.rides" | jq -r '.rows[0][0]'

# Continuous streaming SQL over new records arriving in the WAL
skipprd query --sql "STREAM * FROM bikehire.rides WHERE started_at > CURRENT_TIMESTAMP - INTERVAL '1' HOUR LIMIT 100"
```

In Python, `query()` and `df()` return `pyarrow.Table` objects directly:

```python
import skippr

s = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
table = s.query("SELECT station, count(*) AS rides FROM bikehire.rides GROUP BY station")
print(table.to_pylist())

df = s.df("rides").to_pandas()  # requires skippr[pandas]
```

#### Expose Iceberg REST & Arrow Flight SQL (`skipprd serve`)

To let **Spark, Trino, PyIceberg, DuckDB, or dbt** query your Streamhouse catalog directly, start `skipprd serve`:

```bash
export SKIPPRLAKE_TOKEN="replace-with-a-long-random-token"

# Local loopback (Iceberg REST on :8181, read-only Flight SQL on :8815)
skipprd serve --ready-file ready.json

# Network bind (non-loopback addresses require TLS)
skipprd serve \
  --rest-bind 0.0.0.0:8181 \
  --flight-bind 0.0.0.0:8815 \
  --tls-cert /etc/skipprd/tls/server.crt \
  --tls-key /etc/skipprd/tls/server.key
```

---

### 6. Live Change Data Capture (CDC)

Skippr replicates inserts, updates, and explicit deletes from **PostgreSQL, MySQL, SQL Server, MongoDB, DynamoDB, and Kafka (Debezium)** so destination tables match the current state of your OLTP database.

| `cdc_mode` | Behavior |
|---|---|
| `snapshot` | Copies each selected table once; later runs skip already-loaded tables. |
| `snapshot_then_cdc` | Copies initial table state anchored to the replication coordinate, then streams inserts, updates, and deletes continuously. |
| `cdc_only` | Streams changes from the current log position forward without an initial copy. |

#### Example: PostgreSQL Logical Replication to `SkipprLake`

Ensure your PostgreSQL instance has `wal_level = logical`, a role with `REPLICATION` and `SELECT` privileges, and a publication (`CREATE PUBLICATION skippr_publication FOR TABLE public.customers, public.orders;`). Then configure `type: cdc` and `business_key_columns`:

```yaml
skippr:
  workspace: commerce
  skipprd_el_storage_mode: s3
  skippr_s3_bucket: acme-skippr-metadata
  store:
    type: dynamodb
    name: acme-skippr-store

pipelines:
  orders_cdc:
    type: cdc
    data_source: data_sources.prod_pg
    data_sink: data_sinks.lake
    cdc:
      default:
        business_key_columns: [id]
      namespaces:
        postgres_orders:
          business_key_columns: [order_id]
        postgres_customers:
          business_key_columns: [customer_id]

data_sources:
  prod_pg:
    Postgres:
      host: db.internal
      port: 5432
      user: skippr_reader
      password: ${POSTGRES_PASSWORD}
      database: app
      tables: [customers, orders]
      cdc_mode: snapshot_then_cdc
      publication_name: skippr_publication
      replication_slot_name: skippr_orders_slot

data_sinks:
  lake:
    schema_sink: schema_sinks.lake_schema
    SkipprLake:
      warehouse: s3://acme-iceberg-lake/commerce/
      catalog_table: acme-skippr-store
      region: us-east-1
      table_namespace: replica

schema_sinks:
  lake_schema:
    SkipprLake:
      warehouse: s3://acme-iceberg-lake/commerce/
      catalog_table: acme-skippr-store
      region: us-east-1
      table_namespace: replica
```

Equivalent configuration in Python:

```python
from skippr import (
    CdcNamespaceConfig,
    CdcPipelineConfig,
    Config,
    DataSinkSkipprLake,
    DataSourcePostgres,
    DataSourcePostgresSourceCdcMode,
    DynamoDbStore,
    EnvRef,
    Pipeline,
    PipelineType,
    S3Storage,
)

cfg = (
    Config()
    .workspace("commerce")
    .storage(S3Storage("acme-skippr-metadata"))
    .store(DynamoDbStore("acme-skippr-store"))
)
prod_pg = cfg.data_source(
    "prod_pg",
    DataSourcePostgres(
        host="db.internal",
        user="skippr_reader",
        password=EnvRef("POSTGRES_PASSWORD"),
        database="app",
        tables=["customers", "orders"],
        cdc_mode=DataSourcePostgresSourceCdcMode.SNAPSHOTTHENCDC,
        publication_name="skippr_publication",
        replication_slot_name="skippr_orders_slot",
    ),
)
lake = cfg.data_sink(
    "lake",
    DataSinkSkipprLake(
        warehouse="s3://acme-iceberg-lake/commerce/",
        catalog_table="acme-skippr-store",
        region="us-east-1",
        table_namespace="replica",
    ),
    schema_sink="lake_schema",
)
cfg.pipeline(
    "orders_cdc",
    Pipeline(
        data_source=prod_pg,
        data_sink=lake,
        type=PipelineType.CDC,
        cdc=CdcPipelineConfig(
            default=CdcNamespaceConfig(business_key_columns=["id"]),
            namespaces={
                "postgres_orders": CdcNamespaceConfig(business_key_columns=["order_id"]),
                "postgres_customers": CdcNamespaceConfig(business_key_columns=["customer_id"]),
            },
        ),
    ),
)
cfg.save("skippr.yml")
```

Run discovery and start streaming CDC changes:

```bash
export POSTGRES_PASSWORD="your-replication-password"
skipprd discover --pipeline orders_cdc --log
skipprd sync --pipeline orders_cdc --log
```

Skippr commits each transaction batch to the WAL before recording the PostgreSQL LSN in SkipprStore, applies upserts and positional deletes to Iceberg, and resumes from the last committed LSN after any restart.

---

### 7. Operationalize in Production (Domain Configs, Storage, WAL & SkipprStore)

Moving from a laptop to stateless containers or a multi-node cluster requires no pipeline changes—only operational domain settings.

#### Workspaces & Single-Writer Leases (`skippr.workspace`)

Progress, schemas, and writer locks are keyed by `(skippr.workspace, pipeline_name)`:
- **Isolation & backfills:** Separate environments (`dev`, `staging`, `prod`) or domains with `workspace`. Renaming a workspace or pipeline starts a clean backfill from the beginning.
- **Concurrency safety:** Only one `skipprd sync` process can hold the writer lease for a `(workspace, pipeline_name)` at a time; a second writer waits ~30s and exits safely with `pipeline writer lease unavailable`.

#### Metadata Storage, WAL Backends & SkipprStore

| Layer | Setting | Options | Production Guidance |
|---|---|---|---|
| **Metadata Storage** | `skippr.skipprd_el_storage_mode` / `SKIPPRD_EL_STORAGE_MODE` | `local` (`LocalStorage()`), `s3` (`S3Storage("bucket")`) | Where discovered schemas and HyperLogLog/histogram field stats live. Use `local` (under `DATA_DIR`) on laptops; use `s3` with `skippr_s3_bucket` in production. |
| **WAL Storage** | `WAL_STORAGE` / `--wal-storage` | `disk` *(default)*, `s3`, `clustered` | **`disk`**: local `DATA_DIR` (auto-pauses ingest if free space <5 GB or >90% full, resumes at 80%).<br>**`s3`**: writes WAL segments to `wal_s3_bucket` (`SKIPPR_WAL_S3_BUCKET`) for stateless containers.<br>**`clustered`**: synchronous 2-copy quorum across 3 live nodes for zero-data-loss HA. |
| **SkipprStore** | `skippr.store.type` / `SKIPPR_STORE_TYPE` | `sled` *(default)*, `dynamodb` | Durable KV authority for source offsets, live WAL segment catalog, writer leases, and `SkipprLake` catalog pointers. Pair `WAL_STORAGE=s3` or `clustered` with `dynamodb` (`SKIPPR_STORE_NAME`). |
| **Slice Buffering** | `buffer_threshold_bytes` / `buffer_threshold_seconds` | Default `10485760` (10 MiB) / `60`s | Per-slice (namespace + partition + time bucket) memory flush limits before writing to the WAL. |

#### Warehouse Object Stores (`s3`, Cloudflare `r2`, Local `file`)

`SkipprLake` and `AthenaIceberg` support standard AWS S3 (`type: s3`), S3-compatible object stores like Cloudflare R2 (`type: r2`), and local filesystems (`type: file` on `SkipprLake` and `Duckdb` via `file:///abs/path`):

```yaml
data_sinks:
  r2_lake:
    schema_sink: schema_sinks.r2_lake_schema
    SkipprLake:
      warehouse: s3://acme-r2-lake/warehouse/
      catalog_table: acme-skippr-store
      table_namespace: bronze
      object_store:
        type: r2
        endpoint: ${R2_S3_ENDPOINT}
        region: auto
        access_key_id: ${R2_ACCESS_KEY_ID}
        secret_access_key: ${R2_SECRET_ACCESS_KEY}
        path_style: true

schema_sinks:
  r2_lake_schema:
    SkipprLake:
      warehouse: s3://acme-r2-lake/warehouse/
      catalog_table: acme-skippr-store
      table_namespace: bronze
      object_store:
        type: r2
        endpoint: ${R2_S3_ENDPOINT}
        region: auto
        access_key_id: ${R2_ACCESS_KEY_ID}
        secret_access_key: ${R2_SECRET_ACCESS_KEY}
        path_style: true
```

#### Promoting One `skippr.yml` from Laptop to Stateless Container

Leave `store` and `wal_s3_bucket` unset in `skippr.yml` so environment variables control the runtime topology:

```bash
# Laptop: local WAL and sled store in ./data
export POSTGRES_PASSWORD="dev-password"
skipprd sync --pipeline orders --once

# Stateless container (ECS / Kubernetes / Spot): S3 WAL + DynamoDB SkipprStore
export POSTGRES_PASSWORD="prod-password"
export WAL_STORAGE=s3
export SKIPPR_S3_BUCKET=acme-skippr-metadata
export SKIPPR_WAL_S3_BUCKET=acme-skippr-wal
export SKIPPR_STORE_TYPE=dynamodb
export SKIPPR_STORE_NAME=acme-skippr-store
skipprd sync --pipeline orders --log
```

#### Proving Recovery with Chaos Mode

Set `SKIPPR_CHAOS_MODE=yes` (or `chaos_mode: true` on a pipeline) in staging to kill `skipprd` with `SIGKILL` at random points (15–60s) during sync. Re-run the same `skipprd sync` command to verify automatic WAL recovery and exact row counts.

---

## Connectors

Full configuration guides for every connector are in the [Connector Catalog](https://skippr.io/connectors/).

| Role | Category | Connectors (`skippr.yml` Key) |
|---|---|---|
| **Sources** | Databases & CDC | `Postgres`, `Mysql`, `Mssql`, `Mongodb`, `Dynamodb`, `Clickhouse`, `Redshift`, `Motherduck`, `DeltaLake` |
| **Sources** | Streaming | `Kafka` (incl. Debezium), `Kinesis`, `Sqs`, `Sns`, `Eventbridge`, `Amqp`, `Mqtt`, `Websocket` |
| **Sources** | Files & Storage | `S3`, `Sftp`, `File` |
| **Sources** | Network & Telemetry | `Otlp` (Traces, Logs, Metrics), `HttpClient`, `HttpServer`, `Statsd`, `Socket`, `Pcap`, `Stdin` |
| **Sources** | SaaS, Ads & Finance | `Stripe`, `ShopifyAdmin`, `HubspotCrm`, `GoogleAnalytics` (GA4), `GoogleAds`, `GoogleSearchConsole`, `GooglePageSpeed`, `BingWebmasterTools`, `AppleSearchAds`, `MetaAds`, `MetaInstagramAds`, `LinkedInAds`, `XAds`, `AdrollAds`, `XeroAccounting`, `RevolutBusiness`, `SumUp` |
| **Destinations** | Open Iceberg *(Exactly-Once)* | `SkipprLake`, `AthenaIceberg`, `Duckdb` (paired with matching `schema_sinks`) |
| **Destinations** | Warehouses *(Exactly-Once)* | `Snowflake`, `Bigquery`, `Postgres`, `Redshift`, `Databricks`, `Synapse`, `Motherduck` |
| **Destinations** | Object Storage *(Overwrite-on-Retry)* | `Athena` (with `Glue` schema sink), `S3`, `Gcs`, `AzureBlob`, `Sftp`, `File` |
| **Destinations** | Other | `Clickhouse` *(At-least-once)*, `Amqp` *(At-least-once)*, `Stdout` *(Debug)* |

---

## CLI & SQL Reference

| Command | Purpose |
|---|---|
| `skipprd connect <role> <kind>` | Merge a `data-source`, `data-sink`, or `schema-sink` into `skippr.yml`. |
| `skipprd doctor` | Validate `skippr.yml`, connector links, and `${ENV}` variables (`--output text\|json`). |
| `skipprd discover --pipeline <p>` | Sample source records and infer schemas without writing to destinations. |
| `skipprd schema --pipeline <p>` | Print discovered columns and types. |
| `skipprd metadata show\|apply` | Inspect saved JSON schemas or apply a reviewed schema for a namespace. |
| `skipprd migrate --pipeline <p>` | Apply `migrations/*.sql` `ALTER TABLE` batches in-stream. |
| `skipprd sync [--pipeline <p>] [--once]` | Ingest from source, commit to WAL, and write to destination (`--output progress\|text\|json`). |
| `skipprd query [--sql "<SQL>"]` | Run interactive or scripted (`--plain`, `--watch`) SQL and pipeline management commands. |
| `skipprd df --pipeline <p>` | Output all rows of a pipeline or `--namespace` as JSON. |
| `skipprd serve` | Expose Iceberg REST Catalog (`:8181`) and read-only Flight SQL (`:8815`). |
| `skipprd sql-help` / `benchmark` | Print supported SQL extensions ([`sql-docs.md`](sql-docs.md)) or generate synthetic benchmark data. |

**Management SQL (`skipprd query --sql "..."`):**

```sql
SHOW PIPELINE bikehire;                                            -- Pipeline status, namespaces, offsets, metadata URI
SHOW STATS FOR bikehire.rides;                                     -- HyperLogLog cardinality & histogram stats
SHOW CATALOG FOR bikehire.rides;                                   -- Catalog field definitions
SCHEMA DUMP bikehire TO 'schema.json';                             -- Export schema to JSON file
LOAD SCHEMA 'schema.json' INTO bikehire;                           -- Load schema from JSON file
ALTER TABLE bikehire.rides RENAME COLUMN region TO detail.region;  -- Rename or nest column in-stream
ALTER TABLE bikehire.rides MERGE COLUMN price_string INTO price;   -- Fold sibling column into target
ALTER TABLE bikehire.rides ALTER COLUMN n TYPE BIGINT;             -- Promote column type in-stream
ALTER TABLE bikehire.rides DROP COLUMN legacy_field;               -- Drop column in-stream
DISABLE PIPELINE bikehire;                                         -- Freeze pipeline ingestion
ENABLE PIPELINE bikehire;                                          -- Resume pipeline ingestion
RESET PIPELINE bikehire;                                           -- Purge WAL & offsets to re-ingest from start
DROP PIPELINE bikehire;                                            -- Remove all schemas and local data for pipeline
```

---

## Build from Source

Requires the Rust toolchain pinned in `rust-toolchain.toml`, `protoc` (Protocol Buffers compiler), and OpenSSL development headers (`libssl-dev`).

```bash
cargo build
cargo test --workspace --lib --exclude skipprd-python
./scripts/test-python.sh
cargo fmt --all -- --check
cargo clippy
```

Contributor guidance is in [AGENTS.md](AGENTS.md). Profiling notes are in [PERFORMANCE.md](PERFORMANCE.md). SQL reference is in [sql-docs.md](sql-docs.md).

## License & Ecosystem

This repository is **source-available** under [PolyForm Shield 1.0.0](./LICENSE), not OSI open source.

- **Licensor Line of Business:** Skipprd ELT engine (https://skippr.io)
- **Related:** [Skippr Documentation](https://skippr.io) · [Skippr Data Engineer (`sde`)](https://data-engineer.skippr.io) · [Cloud CLI (`skippr-cloud`)](https://install.skippr.io)
