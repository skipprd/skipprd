# Skippr (`skipprd`) — The Open Streamhouse Engine

[![CI](https://github.com/skipprd/skipprd/actions/workflows/ci.yml/badge.svg)](https://github.com/skipprd/skipprd/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/skipprd/skipprd)](https://github.com/skipprd/skipprd/releases)
[![Documentation](https://img.shields.io/badge/docs-skippr.io-0A66C2)](https://skippr.io)
[![License: PolyForm Shield 1.0.0](https://img.shields.io/badge/license-PolyForm%20Shield%201.0.0-blue)](./LICENSE)

**A warehouse batch job of last night's extract is not enough.**

**Skippr** (`skipprd` CLI and `import skippr` Python SDK) implements the open **Streamhouse** architecture in a single, memory-efficient Rust binary that runs on your own infrastructure. Capture data continuously from OLTP databases (CDC), event streams, files, and SaaS APIs; land atomic, ACID-compliant **Apache Iceberg** tables on Parquet in your own object store; and query a real-time, strongly consistent SQL view across committed Iceberg snapshots and live in-stream rows.

**Your data is never trapped.** Committed Iceberg tables sit in your bucket (`s3://`, Cloudflare R2, or local `file:///`). Query live operational state in Skippr, or read the exact same tables from **Athena, Snowflake, DuckDB, Databricks, Spark, Trino, PyIceberg, and dbt**—with zero second ETL copies and zero vendor lock-in.

**[Full Documentation](https://skippr.io)** · **[Install Guide](https://skippr.io/getting-started/install)** · **[Connector Catalog](https://skippr.io/connectors/)** · **[CLI Reference](https://skippr.io/cli/overview)** · **[Python SDK](https://skippr.io/python)** · **[Enterprise Jumpstart](https://skippr.io/contact)**

---

## Table of Contents

- [Why Skippr?](#why-skippr)
  - [For Data & Technology Leaders](#for-data--technology-leaders)
  - [For Data & Software Engineers](#for-data--software-engineers)
- [How the Open Streamhouse Works](#how-the-open-streamhouse-works)
- [60-Second Quickstart](#60-second-quickstart)
- [Complete Getting Started Guide: From Install to Production](#complete-getting-started-guide-from-install-to-production)
  - [1. Installation & Environment Setup](#1-installation--environment-setup)
  - [2. Configuring Pipelines (`skippr.yml`, CLI & Python)](#2-configuring-pipelines-skippryml-cli--python)
  - [3. Ingest, Write Policies & Deadletter Routing](#3-ingest-write-policies--deadletter-routing)
  - [4. Schema Discovery, In-Stream Evolution & Online Migrations](#4-schema-discovery-in-stream-evolution--online-migrations)
  - [5. Operations & In-Stream Transforms](#5-operations--in-stream-transforms)
  - [6. Querying Real-Time Data & Serving Open Iceberg](#6-querying-real-time-data--serving-open-iceberg)
  - [7. Live Change Data Capture (CDC)](#7-live-change-data-capture-cdc)
  - [8. Operationalizing in Production (Workspaces, Storage Modes, WAL & SkipprStore)](#8-operationalizing-in-production-workspaces-storage-modes-wal--skipprstore)
- [Connector Catalog](#connector-catalog)
- [CLI & SQL Operations Cheat Sheet](#cli--sql-operations-cheat-sheet)
- [Enterprise Jumpstart](#enterprise-jumpstart)
- [Build from Source](#build-from-source)
- [License & Related Products](#license--related-products)

---

## Why Skippr?

Most data stacks force a painful trade-off: stitch together fragile streaming infrastructure (Kafka + Debezium + Flink + Spark + Airflow) over a six-month platform project, or pay a proprietary SaaS ELT vendor to batch-copy rows into a closed warehouse plane where your data is locked in and always hours stale.

Skippr replaces that sprawl with **one self-hosted Streamhouse engine**.

### For Data & Technology Leaders

- **Working Streamhouse in days, not a six-month project:** Drop a single Rust binary (`skipprd`) or Python package (`skippr`) into your VPC, Kubernetes cluster, EC2 instance, or on-prem server. Go from raw operational sources to governed Apache Iceberg tables in minutes.
- **Open architecture — zero vendor lock-in:** Data lands as standard Apache Iceberg on Parquet in buckets you own. Athena, Snowflake, DuckDB, Databricks, Spark, and notebooks query the same files directly. If you ever stop running Skippr, your Iceberg lake remains 100% intact and readable.
- **Full data sovereignty & privacy:** Data never transits a third-party SaaS control plane. Credentials stay in your environment (`${ENV}` references), and payloads move directly from your sources to your storage inside your network perimeter.
- **Lower TCO & compute decoupling:** Stop paying warehouse compute markups just to land, deduplicate, and flatten raw bronze data. Skippr handles schema inference, partitioning, Parquet Z-order/sort clustering, compaction, and snapshot maintenance on lightweight commodity compute before your warehouse ever scans a byte.
- **No 2 AM schema-drift pages:** Upstream APIs and product databases change constantly. Skippr evolves schemas in-stream—adding new columns automatically and isolating incompatible type changes into non-breaking sibling columns (`<field>_<type>`) so pipelines never crash overnight.

### For Data & Software Engineers

- **Dual query path (Real-Time + Historical):** `skipprd query` and `Session.query()` execute DataFusion-powered SQL over **committed Iceberg snapshots unioned with uncommitted, in-flight Write-Ahead Log (WAL) batches**. You see rows the instant they are ingested, while external engines read ACID-committed snapshots.
- **One engine, three ergonomics:** Define and operate pipelines via declarative `skippr.yml`, the `skipprd` CLI (`skipprd connect`, `discover`, `sync`, `query`, `serve`), or the strictly typed Python SDK (`import skippr`, complete with `py.typed` stubs and `pyarrow.Table` outputs). All three read and write the exact same configuration.
- **Ingest that finishes (Crash-Safe WAL & Exactly-Once):** Every batch is durably committed to Skippr's Write-Ahead Log (local disk, S3, or multi-node clustered quorum) before source offsets advance. Even after a `kill -9` (testable anytime with `chaos_mode: true`), the next run recovers from the WAL without losing or duplicating committed rows.
- **Contract-enforced landing semantics:** Each source declares its landing contract (`append`, `merge_by_key`, `replace_partition`, or `replace_table`). If a destination cannot honor the source's contract (such as a mutable lookback window on an append-only sink), Skippr fails closed at startup rather than silently corrupting downstream tables.
- **Online `ALTER TABLE` & SQL migrations without downtime:** Rename columns, move fields into nested structs, widen types, or merge type-conflict sibling columns back in-stream using `skipprd migrate` (`migrations/*.sql`) while preserving Iceberg field IDs.

---

## How the Open Streamhouse Works

```text
  SOURCES                        SKIPPRD STREAMHOUSE ENGINE                      OPEN STORAGE & CONSUMERS
┌───────────┐    ┌────────────────────────────────────────────────────────┐    ┌──────────────────────────┐
│ OLTP CDC  │    │                                                        │    │ Open Apache Iceberg      │
│ Postgres  │───▶│  1. Capture & In-Stream Transforms                     │───▶│ (Parquet + Metadata in   │
│ MySQL/MS  │    │     (Namespace split, flatten, partition, Parquet sort)│    │  S3, R2, or Local Disk)  │
│ Mongo/DDB │    │                          │                             │    └────────────┬─────────────┘
├───────────┤    │                          ▼                             │                 │
│ Streams   │    │  2. Durable Write-Ahead Log (WAL)                      │                 ├──▶ Athena / Snowflake
│ Kafka/SQS │───▶│     (Disk, S3, or Multi-Node Clustered Quorum)         │                 ├──▶ Databricks / Spark
│ Kinesis   │    │          │                               │             │                 ├──▶ DuckDB / PyIceberg
│ OTLP/HTTP │    │          ▼                               ▼             │                 └──▶ skipprd serve
├───────────┤    │  3. SkipprStore                  4. Deadletter Sink    │                      (Iceberg REST +
│ SaaS APIs │───▶│     (Offsets, Leases,               (_dl_<pipeline>    │                       Flight SQL)
│ Stripe    │    │      Catalog Pointers:               for rejected      │
│ GA4 / Ads │    │      Sled or DynamoDB)               payloads)         │    ┌──────────────────────────┐
│ HubSpot   │    │          │                                             │    │ Real-Time SQL Engine     │
├───────────┤    │          └───────────────┬─────────────────────────────┼───▶│ skipprd query / Python   │
│ Files/S3  │───▶│                          ▼                             │    │ (Unions Committed Iceberg│
│ SFTP/Blob │    │  5. In-Stream Schema Evolution & Atomic Commit         │    │  + Live In-Flight WAL)   │
└───────────┘    └────────────────────────────────────────────────────────┘    └──────────────────────────┘
```

1. **Capture & Ingest:** Pull or stream from OLTP databases (snapshot + logical replication CDC), Kafka/SQS/Kinesis, OTLP/HTTP endpoints, S3/SFTP files, and SaaS APIs through on-demand runtime connectors.
2. **In-Stream Schema Evolution:** `skipprd discover` infers nested structs, arrays, and scalar types deterministically. During `skipprd sync`, new fields are added automatically and type conflicts land in `<field>_<type>` sibling columns.
3. **Durable WAL Buffering:** Every batch is committed to the WAL before advancing source offsets or writing to destinations.
4. **Open Iceberg Storage (or Direct Warehouse Sinks):** Atomic snapshots land as standard Parquet data files and Iceberg metadata in your object store (`SkipprLake`, `AthenaIceberg`, `Duckdb`), or load directly into `Snowflake`, `Bigquery`, `Postgres`, `Redshift`, `Databricks`, `Clickhouse`, and `Motherduck`.
5. **Dual Query Path:** Query live state (committed Iceberg + uncommitted WAL) immediately via `skipprd query`, `STREAM * FROM ...`, or Python `Session.query()`, while BI tools, notebooks, and query engines read committed Iceberg tables directly or via `skipprd serve` (Iceberg REST Catalog + Arrow Flight SQL).

---

## 60-Second Quickstart

Install `skipprd`, ingest public bike-hire telemetry from S3 into a local Streamhouse pipeline, and query it with SQL—no external database or cloud warehouse required:

```bash
# 1. Install the CLI (macOS arm64 / Linux x86_64)
brew tap skipprd/tap && brew install skipprd
# Or via install script:
# curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | sh

# 2. Wire an S3 source into a local pipeline (writes skippr.yml)
skipprd --workspace quickstart --storage-mode local connect data-source s3 \
  --pipeline bikehire \
  --name sample \
  --s3-bucket skippr-public-sample-data \
  --s3-prefix bike-hire

# 3. Validate config, discover schema, sync one pass, and run SQL
skipprd doctor
skipprd discover --pipeline bikehire
skipprd sync --pipeline bikehire --once
skipprd query --plain --sql "SELECT count(*) AS total_rides FROM bikehire"
```

Prefer Python? The `skippr` package embeds the exact same Rust engine:

```python
import skippr
from skippr import Config, DataSourceS3, LocalStorage, Pipeline

cfg = Config().workspace("quickstart").storage(LocalStorage())
sample = cfg.data_source(
    "sample",
    DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire"),
)
bikehire = cfg.pipeline("bikehire", Pipeline(data_source=sample))
cfg.save("skippr.yml")

s = skippr.Session(bikehire)
s.discover()
s.sync(once=True)
print(s.query("SELECT count(*) AS total_rides FROM bikehire").to_pylist())
```

---

## Complete Getting Started Guide: From Install to Production

This end-to-end engineering guide walks through every layer of Skippr: installation, declarative configuration, ingestion, schema management, in-stream transforms, real-time SQL querying, live Change Data Capture (CDC), and production operationalization.

---

### 1. Installation & Environment Setup

Skippr publishes precompiled binaries and Python wheels for **macOS arm64** (Apple Silicon) and **Linux x86_64**.

#### Install the CLI (`skipprd`)

```bash
# Option A: Homebrew (macOS & Linux)
brew tap skipprd/tap
brew install skipprd

# Option B: Official install script (writes to /usr/local/bin)
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | sh

# Option C: Non-root install & pinned release version
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh \
  | SKIPPR_INSTALL_DIR="$HOME/.local/bin" SKIPPR_VERSION=6.10.0 sh

# Verify CLI installation
skipprd --version
```

#### Install the Python SDK (`skippr`)

Requires Python 3.10+. Ships with complete `mypy --strict` type stubs (`skippr/__init__.pyi`) and returns `pyarrow.Table` results natively.

```bash
pip install skippr
pip install "skippr[pandas]"   # Optional: enables .to_pandas() on query/df results

python -c "import skippr; print(skippr.Session)"
```

#### How On-Demand Connectors Work

To keep the core engine lightweight, connectors are not statically bloated into the binary. The first time a pipeline references a source or destination, Skippr downloads the matching release connector from `https://install.skippr.io` into `~/.skippr/runtime_plugins` and reuses it on subsequent runs.

For containerized or immutable environments, point the cache to a persistent volume:

```bash
export SKIPPR_RUNTIME_PLUGIN_DIR="/var/lib/skippr/connectors"
```

---

### 2. Configuring Pipelines (`skippr.yml`, CLI & Python)

Every Skippr project is defined by a single `skippr.yml` file that is safe to commit to Git.

#### Root Structure of `skippr.yml`

| Section | Required | Purpose |
|---|---|---|
| `skippr` | Optional | Domain & engine settings: `workspace`, `tenant`, metadata storage mode (`skipprd_el_storage_mode`), state bucket (`skippr_s3_bucket`), WAL bucket (`wal_s3_bucket`), and state store (`store.type`, `store.name`). |
| `pipelines` | Required | Pipeline definitions wiring a `data_source` to an optional `data_sink` and `deadletter_sink`, plus `type` (`ingest`, `cdc`, `copy`), `transform`, `cdc`, and buffer thresholds. |
| `data_sources` | Required | Named source connectors (`Postgres`, `Mysql`, `Mssql`, `Mongodb`, `Dynamodb`, `Kafka`, `S3`, `Stripe`, `GoogleAnalytics`, `Otlp`, etc.). |
| `data_sinks` | Optional | Named destination connectors (`SkipprLake`, `AthenaIceberg`, `Duckdb`, `Snowflake`, `Bigquery`, `Postgres`, `Databricks`, `Clickhouse`, `S3`, etc.). |
| `schema_sinks` | Optional | Catalogs that receive table DDL (`SkipprLake`, `AthenaIceberg`, `Duckdb`, `Glue`, `Snowflake`, `Postgres`, `Bigquery`, etc.). |
| `deadletter_sinks` | Optional | Destinations for records that fail validation, type coercion, or loading. |

> **Tip:** `data_sink` is optional! If you omit `data_sink`, Skippr buffers ingested data durably in the WAL and lets you query it directly with `skipprd query` or `Session.query()`—ideal for exploring a new source before provisioning a destination.

#### Authoring the Same Pipeline in YAML, CLI, or Python

Here is a complete Streamhouse pipeline that reads events from S3 and lands them as Apache Iceberg tables in `SkipprLake`, with a local file deadletter sink:

```yaml
skippr:
  workspace: analytics
  skipprd_el_storage_mode: s3
  skippr_s3_bucket: acme-skippr-metadata
  wal_s3_bucket: acme-skippr-wal
  store:
    type: dynamodb
    name: acme-skippr-store

pipelines:
  bikehire:
    data_source: data_sources.raw_events
    data_sink: data_sinks.lakehouse
    deadletter_sink: deadletter_sinks.rejects
    sync_frequency_seconds: 300

data_sources:
  raw_events:
    S3:
      s3_bucket: skippr-public-sample-data
      s3_prefix: bike-hire

data_sinks:
  lakehouse:
    schema_sink: schema_sinks.lakehouse_schema
    SkipprLake:
      warehouse: s3://acme-iceberg-lake/warehouse/
      catalog_table: acme-skippr-store
      region: us-east-1
      table_namespace: bronze

schema_sinks:
  lakehouse_schema:
    SkipprLake:
      warehouse: s3://acme-iceberg-lake/warehouse/
      catalog_table: acme-skippr-store
      region: us-east-1
      table_namespace: bronze

deadletter_sinks:
  rejects:
    File:
      output_dir: ./deadletters
```

**Building the same config via CLI (`skipprd connect`):**

`skipprd connect` merges entries into `skippr.yml` and validates the entire file before writing:

```bash
skipprd --workspace analytics \
  --storage-mode s3 \
  --skippr-s3-bucket acme-skippr-metadata \
  --wal-s3-bucket acme-skippr-wal \
  --store-type dynamodb \
  --store-name acme-skippr-store \
  connect data-source s3 \
  --pipeline bikehire \
  --name raw_events \
  --s3-bucket skippr-public-sample-data \
  --s3-prefix bike-hire

skipprd connect data-sink skippr-lake \
  --pipeline bikehire \
  --name lakehouse \
  --warehouse 's3://acme-iceberg-lake/warehouse/' \
  --catalog-table acme-skippr-store \
  --region us-east-1 \
  --table-namespace bronze

# Paired Iceberg sinks (SkipprLake, AthenaIceberg, DuckDB) automatically copy their data_sink config
skipprd connect schema-sink skippr-lake \
  --pipeline bikehire \
  --name lakehouse_schema
```

**Building the same config via Python (`skippr.Config`):**

```python
from skippr import (
    Config,
    DataSinkFile,
    DataSinkSkipprLake,
    DataSourceS3,
    DynamoDbStore,
    Pipeline,
    S3Storage,
)

cfg = (
    Config()
    .workspace("analytics")
    .storage(S3Storage("acme-skippr-metadata"))
    .wal_s3_bucket("acme-skippr-wal")
    .store(DynamoDbStore("acme-skippr-store"))
)

raw_events = cfg.data_source(
    "raw_events",
    DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire"),
)

lakehouse = cfg.data_sink(
    "lakehouse",
    DataSinkSkipprLake(
        warehouse="s3://acme-iceberg-lake/warehouse/",
        catalog_table="acme-skippr-store",
        region="us-east-1",
        table_namespace="bronze",
    ),
    schema_sink="lakehouse_schema",
)

rejects = cfg.deadletter_sink(
    "rejects",
    DataSinkFile(output_dir="./deadletters"),
)

cfg.pipeline(
    "bikehire",
    Pipeline(
        data_source=raw_events,
        data_sink=lakehouse,
        deadletter_sink=rejects,
        sync_frequency_seconds=300,
    ),
)
cfg.save("skippr.yml")
```

#### Secret Management & `.env` Precedence

Skippr enforces strict secret hygiene: **secret fields (passwords, API tokens, private keys) reject plaintext strings at config load/save time.**

- In `skippr.yml` and CLI flags, pass whole-value environment references: `'${POSTGRES_PASSWORD}'`.
- In Python, pass `EnvRef("POSTGRES_PASSWORD")`.
- At runtime, Skippr resolves variables in order:
  1. `.env.local` next to `skippr.yml` (highest precedence—gitignore this file for local secrets).
  2. Exported shell environment variables.
  3. `.env` next to `skippr.yml` (fallback defaults).

#### Validating Configuration with `skipprd doctor`

Before running discovery or sync, verify that all connector references, paired schema sinks, storage modes, and environment variables are valid:

```bash
skipprd doctor
# Or for machine-readable CI validation:
skipprd doctor --output json
```

---

### 3. Ingest, Write Policies & Deadletter Routing

#### Running Ingestion (`skipprd sync`)

Once your pipeline is configured and its schema is discovered, run `skipprd sync`:

```bash
# Run a single ingestion pass and exit (ideal for cron, Airflow, Dagster, or CI)
skipprd sync --pipeline bikehire --once --log

# Run continuously as a daemon (polls every sync_frequency_seconds, default 900s)
skipprd sync --pipeline bikehire --log

# Run all pipelines defined in skippr.yml in sequence
skipprd sync --once --log
```

During each sync pass, Skippr:
1. Reads a batch from the source.
2. Commits the batch to the durable **Write-Ahead Log (WAL)**.
3. Advances and records the source offset in **SkipprStore** so a restart resumes from that exact position.
4. Writes committed batches to the destination (`data_sink`), evolving destination tables in-stream if new fields arrived.
5. Reclaims WAL segments only after the destination acknowledges the commit.

#### Pipeline Modes (`type`)

Every pipeline runs in one of three modes (`pipelines.<name>.type`):

- `ingest` *(default)*: Parses records into Apache Arrow batches, infers and evolves schemas, applies transforms, and lands structured tables in Iceberg or warehouses.
- `cdc`: Streams and applies Change Data Capture inserts, updates, and explicit deletes using business keys (`PipelineType.CDC`).
- `copy`: High-throughput identity transfer (`PipelineType.COPY`) that moves files/objects directly to `File`, `S3`, `Gcs`, `AzureBlob`, or `Sftp` sinks while tracking offsets and routing malformed records to Arrow deadletters.

#### Source Landing Semantics & Write Policies

Sources don't just emit columns—they declare **how each table must be landed** so mutable upstream APIs never silently corrupt downstream analytics:

| Write Policy | Behavior | Typical Sources |
|---|---|---|
| `append` | Appends new rows on every pass. | Immutable event streams, logs, S3 files, Kafka |
| `merge_by_key` | Upserts rows on the table's primary/business key. | OLTP CDC (`Postgres`, `Mysql`, `Mssql`), CRM snapshots |
| `replace_partition` | Atomically drops and rewrites the partition slice (e.g., `date=2026-10-24`) on each run. | Mutable reporting APIs with lookback windows (`GoogleAnalytics` GA4, Ads APIs) |
| `replace_table` | Atomically replaces the bounded table with the latest snapshot. | Small reference tables or full snapshots |

**Fail-closed safety:** If a source requires `replace_partition` (for example, GA4 refreshing its `lookback_days` window) and you point it at an append-only destination, Skippr refuses to start with a clear error rather than duplicating revised days. Both `SkipprLake` and `AthenaIceberg` support all four policies (`append`, `merge_by_key`, `replace_partition`, `replace_table`) natively via atomic Iceberg commits.

#### Deadletter Routing & Inspection

When a record is malformed or fails type coercion, Skippr never aborts the batch or drops valid rows. Instead, it wraps the raw payload, source URI, resume offset, failure code, and error message into a **deadletter record** and sends it to `deadletter_sink`.

Query deadletters directly with SQL via `_dl_<pipeline>`:

```bash
skipprd query --sql "
  SELECT id, namespace, failure_code, error, processed_time
  FROM _dl_bikehire
  WHERE namespace = 'rides'
  ORDER BY processed_time DESC
  LIMIT 25
"
```

---

### 4. Schema Discovery, In-Stream Evolution & Online Migrations

With Skippr, you never write `CREATE TABLE` DDL by hand.

#### Step 4.1: Discover and Inspect the Schema

`skipprd discover` connects to your source, samples records without writing anything to your destination, and infers field names, types, nested structs (`record`), arrays (`array`), and table namespaces:

```bash
# Infer schema from the source
skipprd discover --pipeline bikehire --log

# Print the human-readable schema contract
skipprd schema --pipeline bikehire

# Export the structured JSON schema metadata
skipprd metadata show --pipeline bikehire
```

#### Step 4.2: Review, Pin, or Override Schemas in CI

Inference is deterministic, including automatic detection of ISO-8601 dates and Unix timestamps. If you want to review or override a type before first sync (for example, forcing a numeric-looking string ID to `long` or marking a primary key `nullable: false`), use `skipprd metadata apply`:

```bash
# 1. Export the discovered fields for the 'rides' namespace
skipprd metadata show --pipeline bikehire \
  | jq '.namespaces[] | select(.namespace == "rides") | .fields' \
  > rides-schema.json

# 2. Apply a reviewed or edited schema back to the pipeline
skipprd metadata apply --pipeline bikehire --namespace rides --schema rides-schema.json

# Or apply inline JSON:
skipprd metadata apply --pipeline bikehire --namespace rides \
  --schema-json '{"fields":[{"name":"ride_id","field_type":"long","nullable":false},{"name":"station","field_type":"string"}]}'
```

To require manual review of schema changes in production, set `SCHEMA_AUTO_APPROVE=false` (or `auto_approve: false` on the pipeline).

#### Step 4.3: Automatic In-Stream Schema Evolution

When upstream data drifts during `skipprd sync`:
- **New fields** are automatically added to the pipeline schema and destination table in-stream.
- **Compatible values** (such as an integer arriving in a `double` column) coerce cleanly into the existing column.
- **Incompatible types (Type Drift)** never break existing queries or drop the row: Skippr automatically creates a **sibling column** named `<field>_<type>`. For example, if `price` is a `Double` and a record arrives with `"price": "free"`, Skippr writes `"free"` to `price_string` and keeps the pipeline running.

#### Step 4.4: Online `ALTER TABLE` & Schema Migrations (`skipprd migrate`)

After data has landed in Iceberg (`SkipprLake`, `AthenaIceberg`, `Duckdb`) or a native-DDL warehouse (`Snowflake`, `Postgres`, `Bigquery`, `Redshift`, `Motherduck`), you can evolve columns **in-stream without stopping the pipeline** while preserving Iceberg field IDs:

```bash
# Fold a type-conflict sibling column back into the main column
skipprd query --sql "ALTER TABLE bikehire.rides MERGE COLUMN price_string INTO price"

# Rename a column or move a top-level column into a nested struct
skipprd query --sql "ALTER TABLE bikehire.rides RENAME COLUMN region TO detail.region"

# Widen a column type (Iceberg-safe promotion: byte/short/int -> BIGINT, float -> DOUBLE)
skipprd query --sql "ALTER TABLE bikehire.rides ALTER COLUMN duration_sec TYPE BIGINT"

# Drop a legacy column
skipprd query --sql "ALTER TABLE bikehire.rides DROP COLUMN legacy_temp_flag"
```

For version-controlled, idempotent migrations across environments, author SQL files in `migrations/*.sql` (either by hand or with Python `skippr.Migration`) and apply them with `skipprd migrate`:

```python
import skippr
from skippr import Config, Migration

Migration(
    "20261024_curate_rides_schema",
    up="ALTER TABLE bikehire.rides RENAME COLUMN region TO region_code;",
    down="ALTER TABLE bikehire.rides RENAME COLUMN region_code TO region;",
).write("migrations")

session = skippr.Session(Config.discover().get_pipeline("bikehire"))
session.migrate()
```

```bash
# Or apply migrations from the CLI:
skipprd migrate --pipeline bikehire
```

---

### 5. Operations & In-Stream Transforms

Skippr transforms records in-flight between the source and the destination—splitting multiplexed streams into separate tables, unwrapping JSON envelopes, flattening nested structs, partitioning folders, and sorting Parquet row groups for fast predicate pushdown.

#### Complete Transform Configuration

```yaml
pipelines:
  telemetry:
    data_source: data_sources.event_stream
    data_sink: data_sinks.lakehouse
    transform:
      # 1. Unwrap records from a JSON envelope array (e.g. {"page": 1, "data": [...]})
      record_field_path: data
      # 2. Split one mixed stream into separate tables & schemas per distinct value
      namespace_fields: event_type
      # 3. Flatten nested objects into dot-separated columns (e.g. user.address.city)
      flatten_events: true
      # 4. Enrich every record with static metadata columns
      inject_fields:
        environment: production
        ingested_by: skipprd-eu-west-1
      # 5. Hive-style field partitioning with an allowlist
      batch_partition_fields: country
      partition_allowed_values: GB,US,DE,FR
      # 6. Time-based partitioning (year/month/day/hour/minute)
      batch_time_fields: event_timestamp,created_at
      batch_time_unit: day
      time_partition_prefix: p_
      # 7. Sort rows inside each Parquet file for min/max row-group pruning
      batch_order_fields: customer_id,event_timestamp

data_sources:
  event_stream:
    S3:
      s3_bucket: acme-raw-events
      s3_prefix: stream

data_sinks:
  lakehouse:
    schema_sink: schema_sinks.lakehouse_schema
    SkipprLake:
      warehouse: s3://acme-iceberg-lake/warehouse/
      catalog_table: acme-skippr-store

schema_sinks:
  lakehouse_schema:
    SkipprLake:
      warehouse: s3://acme-iceberg-lake/warehouse/
      catalog_table: acme-skippr-store
```

**Equivalent Transform in Python:**

```python
from skippr import BatchTimeUnit, Config, Pipeline, Transform

cfg = Config.load("skippr.yml")
cfg.pipeline(
    "telemetry",
    Pipeline(
        data_source=cfg.get_data_source("event_stream"),
        data_sink=cfg.get_data_sink("lakehouse"),
        transform=Transform(
            record_field_path="data",
            namespace_fields="event_type",
            flatten_events=True,
            inject_fields={"environment": "production", "ingested_by": "skipprd-eu-west-1"},
            batch_partition_fields="country",
            partition_allowed_values="GB,US,DE,FR",
            batch_time_fields="event_timestamp,created_at",
            batch_time_unit=BatchTimeUnit.DAY,
            time_partition_prefix="p_",
            batch_order_fields="customer_id,event_timestamp",
        ),
    ),
)
cfg.save()
```

#### Why `batch_order_fields` Matters for Query Performance

When `batch_order_fields` is set (for example, `customer_id,event_timestamp`), Skippr sorts rows inside each Parquet file before writing and automatically sizes row groups between 16 MiB and 64 MiB uncompressed based on row width and leading-column cardinality. Because Parquet records `min` and `max` statistics per row group, downstream engines (Skippr, Athena, Snowflake, DuckDB, Spark) can skip 95%+ of row groups on selective `WHERE customer_id = ...` queries.

---

### 6. Querying Real-Time Data & Serving Open Iceberg

Skippr's Streamhouse architecture gives you a **dual query path**: real-time operational SQL inside Skippr, and open Iceberg consumption from any external engine.

#### Path A: Real-Time SQL in Skippr (`skipprd query` & `STREAM`)

When you run `skipprd query` against an Iceberg destination (`SkipprLake`, `AthenaIceberg`, `Duckdb`), Skippr automatically unions the **committed Iceberg table** with **live, uncommitted batches sitting in the WAL**. You get strongly consistent, up-to-the-second query results without waiting for warehouse batch jobs.

```bash
# 1. Interactive SQL shell
skipprd query

# 2. Single-shot analytical query
skipprd query --sql "
  SELECT station, COUNT(*) AS rides, AVG(duration_s) AS avg_duration
  FROM bikehire.rides
  WHERE started_at >= '2026-01-01'
  GROUP BY station
  ORDER BY rides DESC
  LIMIT 10
"

# 3. Live terminal watch mode (re-evaluates every 5 seconds during a running sync)
skipprd query --sql "SELECT count(*) AS live_rows FROM bikehire.rides" --watch 5

# 4. Scriptable JSON output for CI and automation
skipprd query --plain --sql "SELECT count(*) AS rides FROM bikehire.rides" | jq -r '.rows[0][0]'

# 5. Continuous streaming SQL tailing the live WAL as events arrive
skipprd query --sql "
  STREAM user_id, station, started_at
  FROM bikehire.rides
  WHERE started_at > CURRENT_TIMESTAMP - INTERVAL '1' HOUR
  ORDER BY started_at
  LIMIT 100
"
```

#### Querying into PyArrow & Pandas from Python

```python
import skippr

session = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))

# Execute arbitrary SQL and receive a pyarrow.Table
top_stations = session.query(
    "SELECT station, count(*) AS rides FROM bikehire.rides GROUP BY station ORDER BY rides DESC LIMIT 5"
)
print(top_stations.to_pylist())

# Or pull an entire namespace as a pyarrow.Table / pandas DataFrame
arrow_table = session.df("rides")
df = arrow_table.to_pandas()  # requires pip install "skippr[pandas]"
```

#### Path B: Serving External Engines via Iceberg REST & Flight SQL (`skipprd serve`)

Want **Spark, Trino, PyIceberg, DuckDB, or dbt** to read your Streamhouse tables? `skipprd serve` exposes two standard endpoints over your Iceberg catalog (`SkipprLake`, `AthenaIceberg`, or `Duckdb`):
1. **Iceberg REST Catalog** (default `127.0.0.1:8181`)
2. **Read-Only Arrow Flight SQL** (default `127.0.0.1:8815`)

```bash
export SKIPPRLAKE_TOKEN="replace-with-a-long-random-bearer-token"

# Serve locally for notebooks, dbt, or PyIceberg
skipprd --config skippr.yml serve --ready-file ready.json

# Serve over a network interface (non-loopback binds enforce TLS)
skipprd serve \
  --rest-bind 0.0.0.0:8181 \
  --flight-bind 0.0.0.0:8815 \
  --tls-cert /etc/skipprd/tls/server.crt \
  --tls-key /etc/skipprd/tls/server.key \
  --ready-file /run/skipprd/ready.json
```

And because the underlying files in your bucket are standard Apache Iceberg on Parquet, engines with native catalog integration (AWS Athena via Glue, Snowflake Open Catalog, Databricks, DuckDB `iceberg_scan`) can also read the files directly from your object store with zero Skippr compute in the read path.

---

### 7. Live Change Data Capture (CDC)

Periodic batch snapshots miss intermediate deletes, hammer production databases with full-table scans, and leave analytics hours behind reality. Skippr's native CDC engine streams inserts, updates, and explicit deletes from **PostgreSQL, MySQL, SQL Server, MongoDB, DynamoDB, and Kafka (Debezium)** directly into your Streamhouse.

#### Choosing a `cdc_mode`

| `cdc_mode` | What It Does | When to Use |
|---|---|---|
| `snapshot` | Copies each table once; subsequent runs skip already-loaded tables. | One-time migration or initial backfill. |
| `snapshot_then_cdc` | Performs a consistent initial table copy anchored to the replication coordinate, then continuously streams inserts, updates, and deletes. | **Recommended for production:** full historical state followed by live replication. |
| `cdc_only` | Streams changes from the current log position forward without an initial table copy. | Target table already has historical data, or you only want new change events. |

#### Step-by-Step: PostgreSQL Logical Replication to Apache Iceberg (`SkipprLake`)

**1. Prepare PostgreSQL (one-time admin setup):**

Ensure `wal_level = logical` is enabled on your PostgreSQL instance, grant replication privileges, and create a publication for the tables you want to replicate (every replicated table should have a primary key or `REPLICA IDENTITY FULL`):

```sql
CREATE ROLE skippr_reader WITH LOGIN REPLICATION PASSWORD 'change-me';
GRANT CONNECT ON DATABASE app TO skippr_reader;
GRANT USAGE ON SCHEMA public TO skippr_reader;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO skippr_reader;

CREATE PUBLICATION skippr_publication FOR TABLE public.customers, public.orders;
```

**2. Configure the CDC Pipeline (`type: cdc` + business key contracts):**

In `skippr.yml`, set `type: cdc` on the pipeline, configure `cdc_mode: snapshot_then_cdc` on the PostgreSQL source, and declare `business_key_columns` so the destination applies upserts and explicit deletes deterministically:

```yaml
skippr:
  workspace: prod_commerce
  skipprd_el_storage_mode: s3
  skippr_s3_bucket: acme-skippr-metadata
  store:
    type: dynamodb
    name: acme-skippr-store

pipelines:
  commerce_cdc:
    type: cdc
    data_source: data_sources.prod_postgres
    data_sink: data_sinks.iceberg_lake
    cdc:
      default:
        business_key_columns: [id]
      namespaces:
        postgres_orders:
          business_key_columns: [order_id]
        postgres_customers:
          business_key_columns: [customer_id]

data_sources:
  prod_postgres:
    Postgres:
      host: pg-prod.internal
      port: 5432
      user: skippr_reader
      password: ${POSTGRES_PASSWORD}
      database: app
      tables: [customers, orders]
      cdc_mode: snapshot_then_cdc
      publication_name: skippr_publication
      replication_slot_name: skippr_commerce_slot

data_sinks:
  iceberg_lake:
    schema_sink: schema_sinks.iceberg_lake_schema
    SkipprLake:
      warehouse: s3://acme-iceberg-lake/commerce/
      catalog_table: acme-skippr-store
      region: us-east-1
      table_namespace: prod_replica

schema_sinks:
  iceberg_lake_schema:
    SkipprLake:
      warehouse: s3://acme-iceberg-lake/commerce/
      catalog_table: acme-skippr-store
      region: us-east-1
      table_namespace: prod_replica
```

**Equivalent CDC Pipeline in Python:**

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
    .workspace("prod_commerce")
    .storage(S3Storage("acme-skippr-metadata"))
    .store(DynamoDbStore("acme-skippr-store"))
)

prod_pg = cfg.data_source(
    "prod_postgres",
    DataSourcePostgres(
        host="pg-prod.internal",
        port=5432,
        user="skippr_reader",
        password=EnvRef("POSTGRES_PASSWORD"),
        database="app",
        tables=["customers", "orders"],
        cdc_mode=DataSourcePostgresSourceCdcMode.SNAPSHOTTHENCDC,
        publication_name="skippr_publication",
        replication_slot_name="skippr_commerce_slot",
    ),
)

iceberg_lake = cfg.data_sink(
    "iceberg_lake",
    DataSinkSkipprLake(
        warehouse="s3://acme-iceberg-lake/commerce/",
        catalog_table="acme-skippr-store",
        region="us-east-1",
        table_namespace="prod_replica",
    ),
    schema_sink="iceberg_lake_schema",
)

cfg.pipeline(
    "commerce_cdc",
    Pipeline(
        data_source=prod_pg,
        data_sink=iceberg_lake,
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

**3. Discover, Stream Changes, and Query Current State:**

```bash
export POSTGRES_PASSWORD="your-replication-password"

# Discover initial schemas
skipprd discover --pipeline commerce_cdc --log

# Run continuous CDC replication (or add --once with cdc_idle_timeout_seconds for micro-batching)
skipprd sync --pipeline commerce_cdc --log
```

**CDC Guarantees You Can Rely On:**
- **Exact final state:** If a row is inserted, updated twice, and deleted, the Iceberg table reflects the exact current state—including explicit deletes via Iceberg positional delete files and compaction.
- **Strict LSN/offset ordering:** Skippr records the PostgreSQL LSN (or binlog/stream offset) only *after* the batch is durably committed to the WAL. Older versions never overwrite newer versions.
- **Zero-touch crash recovery:** If the runner restarts, `skipprd sync` drains any uncommitted WAL segments and resumes replication from the exact LSN committed in SkipprStore.

---

### 8. Operationalizing in Production (Workspaces, Storage Modes, WAL & SkipprStore)

When moving from a developer laptop to production containers, Kubernetes, or multi-node clusters, you do not rewrite your pipelines. You configure Skippr's **operational domain settings**.

#### 8.1 Workspaces, Tenants & Pipeline Identity

Every pipeline's state, offset checkpoints, and writer lease are scoped by the composite key `(skippr.workspace, pipeline_name)`:

- **Environment isolation (`workspace`):** Use distinct workspaces (e.g., `dev`, `staging`, `prod` or domain-specific workspaces like `finance`, `product_analytics`) to isolate state cleanly.
- **Single-writer lease protection:** Two `skipprd sync` processes for the same `(workspace, pipeline_name)` cannot corrupt state by writing concurrently. The second process waits ~30 seconds for the distributed writer lease and exits safely with `pipeline writer lease unavailable`.
- **Clean backfills via rename:** Because progress is bound to `(workspace, pipeline_name)`, changing either name treats the pipeline as brand new and backfills from the beginning.

#### 8.2 Metadata Storage Modes (`skipprd_el_storage_mode`)

Controls where Skippr persists discovered schemas, uploaded run configs, and per-field HyperLogLog/histogram statistics:

| Mode | YAML (`skippr:`) | Python (`Config.storage(...)`) | When to Use |
|---|---|---|---|
| **Local** | `skipprd_el_storage_mode: local` | `LocalStorage()` | Local development, CI tests, and single-machine setups without S3. Stores metadata under `DATA_DIR` (`./data`). |
| **S3** *(default)* | `skipprd_el_storage_mode: s3` + `skippr_s3_bucket: <bucket>` | `S3Storage("<bucket>")` | Production deployments where schema contracts and stats must be shared across runners and preserved independently of local disk. |

#### 8.3 Write-Ahead Log (WAL) Backends (`WAL_STORAGE`) & Buffering

The WAL backend is a machine-level setting configured via the `WAL_STORAGE` environment variable or `--wal-storage` CLI flag:

| `WAL_STORAGE` | Where WAL Segments Live | When to Use |
|---|---|---|
| `disk` *(default)* | Local persistent directory `DATA_DIR` (default `./data`). Includes automatic disk backpressure (pauses ingest if free space drops below 5 GB or >90% full, resumes at 80%). | Long-lived VMs, bare-metal servers, or StatefulSets with persistent volumes. |
| `s3` | Dedicated S3 bucket (`skippr.wal_s3_bucket` or `SKIPPR_WAL_S3_BUCKET`). Zero state on local disk after segment flush. | Ephemeral compute: stateless ECS tasks, Kubernetes Deployments, Spot instances, or scheduled batch runners. |
| `clustered` | Synchronous multi-node quorum across local disks + remote `SkipprStore` leases (requires 3 live `skipprd sync` nodes to tolerate a node failure with zero data loss). | Mission-critical streaming pipelines requiring continuous high availability across hardware failures. |

**Tuning In-Memory Slice Buffering:**
Skippr buffers records in memory per slice (namespace + partition + time bucket) and flushes to the WAL when either threshold is reached:
- `buffer_threshold_bytes` (`BUFFER_THRESHOLD_BYTES`, default `10485760` / 10 MiB)
- `buffer_threshold_seconds` (`BUFFER_THRESHOLD_SECONDS`, default `60` seconds)

#### 8.4 SkipprStore State Backends (`skippr.store`)

**SkipprStore** is the durable key-value authority for source resume offsets, live WAL segment catalogs, distributed writer leases, and `SkipprLake` Iceberg catalog pointers:

| Store Type | YAML (`skippr.store:`) | Python (`Config.store(...)`) | When to Use |
|---|---|---|---|
| `sled` *(default)* | `type: sled` | `SledStore()` | Single-host deployments paired with `WAL_STORAGE=disk` and a persistent `DATA_DIR`. |
| `dynamodb` | `type: dynamodb` + `name: <table>` | `DynamoDbStore("<table>")` | Ephemeral compute (`WAL_STORAGE=s3`), multi-node `WAL_STORAGE=clustered`, or `SkipprLake` catalogs. Offsets, leases, and Iceberg catalog pointers can share **one** DynamoDB table safely via non-colliding key prefixes. |

#### 8.5 Iceberg Warehouse Object Store Backends (`object_store`)

`SkipprLake` and `AthenaIceberg` support multiple storage backends for your Parquet and Iceberg metadata files:

- **AWS S3 (`type: s3`, default):** Uses `warehouse: s3://bucket/prefix/` and standard AWS credential chains.
- **Cloudflare R2 / S3-Compatible (`type: r2`):** Zero-egress Iceberg lakes using custom endpoints and path-style access.
- **Local Filesystem (`type: file`, `SkipprLake` & `Duckdb`):** Uses `warehouse: file:///var/lib/skipprd/lake` for local development, edge nodes, or air-gapped servers.

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

#### 8.6 One `skippr.yml`, Any Environment (Dev Laptop vs. Production Cluster)

Because Skippr resolves settings with deterministic precedence (`skippr.yml` → CLI flags → environment variables → defaults), you can keep your `skippr.yml` clean and promote the exact same file from laptop to production:

```bash
# On a developer laptop (uses local disk WAL and local sled store in ./data)
export POSTGRES_PASSWORD="dev-password"
skipprd sync --pipeline orders --once

# In a stateless production container (S3 WAL + DynamoDB SkipprStore)
export POSTGRES_PASSWORD="prod-password"
export WAL_STORAGE=s3
export SKIPPR_WAL_S3_BUCKET=acme-prod-skipprd-wal
export SKIPPR_STORE_TYPE=dynamodb
export SKIPPR_STORE_NAME=acme-prod-skipprd-store
skipprd sync --pipeline orders --log
```

#### 8.7 Proving Crash Recovery with Chaos Mode

Don't take crash recovery on faith. Enable `SKIPPR_CHAOS_MODE=yes` (or `chaos_mode: true` in `skippr.yml`) in your staging environment to intentionally terminate `skipprd` with `SIGKILL` at random intervals (15–60s) mid-sync. Re-run `skipprd sync` and verify that source and destination row counts match with zero data loss and zero duplicates.

---

## Connector Catalog

All connectors are downloaded on demand and configured via `skippr.yml`, `skipprd connect`, or `skippr.Config`. See the full [Connector Catalog](https://skippr.io/connectors/) for per-connector options.

### Data Sources (`data_sources`)

| Category | Connectors (`skippr.yml` Type) |
|---|---|
| **Databases & CDC** | `Postgres` (PostgreSQL), `Mysql` (MySQL), `Mssql` (SQL Server), `Mongodb` (MongoDB), `Dynamodb` (DynamoDB), `Clickhouse` (ClickHouse), `Redshift` (Amazon Redshift), `Motherduck` (MotherDuck), `DeltaLake` (Delta Lake) |
| **Streaming & Messaging** | `Kafka` (Apache Kafka + Debezium), `Kinesis` (AWS Kinesis), `Sqs` (AWS SQS), `Sns` (AWS SNS), `Eventbridge` (AWS EventBridge), `Amqp` (RabbitMQ / AMQP), `Mqtt` (MQTT), `Websocket` (WebSocket) |
| **Files & Object Storage** | `S3` (AWS S3 / S3-compatible), `Sftp` (SFTP), `File` (Local filesystem) |
| **Telemetry, HTTP & Network** | `Otlp` (OpenTelemetry Traces, Logs & Metrics), `HttpClient` (REST / Paginated APIs), `HttpServer` (Webhook receiver), `Statsd` (StatsD), `Socket` (TCP/UDP Socket), `Pcap` (Packet capture), `Stdin` (Standard input) |
| **Marketing, Ads & Search** | `GoogleAnalytics` (GA4), `GoogleAds`, `GoogleSearchConsole`, `GooglePageSpeed`, `BingWebmasterTools`, `AppleSearchAds`, `MetaAds`, `MetaInstagramAds`, `LinkedInAds`, `XAds`, `AdrollAds` |
| **Commerce, Finance & CRM** | `Stripe`, `ShopifyAdmin`, `HubspotCrm`, `XeroAccounting`, `RevolutBusiness`, `SumUp` |

### Destinations (`data_sinks`) & Schema Sinks (`schema_sinks`)

| Category | Connectors (`skippr.yml` Type) | Retry & Delivery Guarantee |
|---|---|---|
| **Open Iceberg Streamhouse** | `SkipprLake` (+ `SkipprLake` schema sink), `AthenaIceberg` (+ `AthenaIceberg` schema sink), `Duckdb` (+ `Duckdb` schema sink) | **Exactly-once** atomic Iceberg snapshot commits |
| **Cloud Data Warehouses** | `Snowflake`, `Bigquery`, `Postgres`, `Redshift`, `Databricks`, `Synapse`, `Motherduck` (with matching schema sinks) | **Exactly-once** idempotent batch apply |
| **Hive & Cloud Object Storage** | `Athena` (+ `Glue` schema sink), `S3`, `Gcs`, `AzureBlob`, `Sftp`, `File` | **Overwrite-on-retry** deterministic paths |
| **Analytical & Messaging Sinks** | `Clickhouse`, `Amqp`, `Stdout` | **At-least-once** (`Clickhouse`, `Amqp`) / Debug (`Stdout`) |

---

## CLI & SQL Operations Cheat Sheet

### CLI Commands

| Command | Description |
|---|---|
| `skipprd connect data-source <kind>` | Add or update a source connector in `skippr.yml` and link it to `--pipeline`. |
| `skipprd connect data-sink <kind>` | Add or update a destination connector in `skippr.yml` and link it to `--pipeline`. |
| `skipprd connect schema-sink <kind>` | Add or update a catalog schema sink and link it to the pipeline's `data_sink`. |
| `skipprd doctor` | Validate `skippr.yml`, connector pairings, storage settings, and `${ENV}` variables. |
| `skipprd discover --pipeline <name>` | Sample the source and infer schemas without writing to the destination. |
| `skipprd schema --pipeline <name>` | Display discovered columns and types for a pipeline. |
| `skipprd metadata show --pipeline <name>` | Print saved per-namespace schema metadata as JSON. |
| `skipprd metadata apply ...` | Replace a namespace's schema from a reviewed JSON file or stdin. |
| `skipprd migrate --pipeline <name>` | Apply idempotent `migrations/*.sql` `ALTER TABLE` batches in-stream. |
| `skipprd sync [--pipeline <name>] [--once]` | Ingest from sources, commit to the WAL, and land data in destinations. |
| `skipprd query [--sql "<SQL>"] [--plain]` | Query committed Iceberg + live WAL data, or run pipeline management SQL. |
| `skipprd df --pipeline <name>` | Emit all rows of a pipeline or `--namespace` as JSON. |
| `skipprd serve` | Serve Iceberg tables over an Iceberg REST Catalog (`:8181`) and Arrow Flight SQL (`:8815`). |
| `skipprd sql-help` | Print documentation for all supported Skippr SQL statements (`sql-docs.md`). |
| `skipprd benchmark` | Generate synthetic JSON datasets for throughput and latency benchmarking. |

### Built-In SQL Management Statements (`skipprd query --sql "..."`)

```sql
-- Inspect pipeline status, offsets, and field statistics
SHOW PIPELINE bikehire;
SHOW STATS FOR bikehire.rides;
SHOW CATALOG FOR bikehire.rides;
SHOW SEMANTIC FOR bikehire.rides;

-- Export and load schemas
SCHEMA DUMP bikehire TO 'bikehire-schema.json';
LOAD SCHEMA 'bikehire-schema.json' INTO bikehire;

-- In-stream table evolution
ALTER TABLE bikehire.rides RENAME COLUMN region TO detail.region;
ALTER TABLE bikehire.rides MERGE COLUMN price_string INTO price;
ALTER TABLE bikehire.rides ALTER COLUMN n TYPE BIGINT;
ALTER TABLE bikehire.rides DROP COLUMN legacy_field;

-- Freeze, resume, reset, or drop pipelines
DISABLE PIPELINE bikehire;
ENABLE PIPELINE bikehire;
RESET PIPELINE bikehire;
DROP PIPELINE bikehire;
```

---

## Enterprise Jumpstart

Skippr works completely out of the box—you never need a sales call to land Apache Iceberg tables in your own bucket.

For engineering and data leadership teams that want to accelerate production rollout, **Enterprise Jumpstart** is an optional onboarding track where a Skippr founder-engineer works directly with your team to deploy `skipprd` inside your VPC, wire your first production CDC and SaaS pipelines, configure S3/DynamoDB/clustered high availability, and hand you the keys.

- **[Book a 20-minute architecture call](https://skippr.io/contact)**

---

## Build from Source

Building `skipprd` from source requires the Rust toolchain pinned in `rust-toolchain.toml` (Rust 1.88.0), `protoc` (Protocol Buffers compiler), and OpenSSL development headers (`libssl-dev`).

```bash
# Build the engine binary
cargo build

# Run the Rust library test suite
cargo test --workspace --lib --exclude skipprd-python

# Build and test the Python wheel + mypy --strict stub checks
./scripts/test-python.sh

# Check formatting and lints
cargo fmt --all -- --check
cargo clippy
```

Contributor architecture and release conventions are documented in [AGENTS.md](AGENTS.md). Profiling and Linux kernel/runtime observations are in [PERFORMANCE.md](PERFORMANCE.md). Full SQL extension syntax is in [sql-docs.md](sql-docs.md).

---

## License & Related Products

This repository is **source-available** under the [PolyForm Shield License 1.0.0](./LICENSE) (not OSI open source). You may use, modify, and redistribute the software for any purpose **except** providing a commercial or free product that competes with Skippr.

- **Licensor Line of Business:** Skippr Cloud, Skippr Data Engineer, Skipprd ELT engine, Skippr ReAct, Skippr IDE, and SkipprDB (https://skippr.io)
- **Required Notice:** Copyright Skippr Ltd (https://skippr.io)
- **Related Skippr Products:**
  - **Skippr Documentation & Streamhouse Guides:** [skippr.io](https://skippr.io)
  - **Skippr Data Engineer (`sde`):** [data-engineer.skippr.io](https://data-engineer.skippr.io)
  - **Skippr Cloud CLI (`skippr-cloud`):** [install.skippr.io](https://install.skippr.io)
