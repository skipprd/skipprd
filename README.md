# Skippr

Skippr is a Rust CLI and runtime for data ingestion, warehouse modeling, and data-engineering workflows configured from one `skippr.yml`.

The repository ships two useful binaries:

- `skippr` — the product CLI for project setup, connector configuration, shared engine commands, modeling, dbt helpers, vector ingestion, chat, and diagnostics.
- `skipprd` — the lightweight engine/runtime binary for discovery, sync, engine query/schema operations, Lambda images, and runtime plugin testing.

Both binaries read the same `skippr.yml` for shared engine commands such as `discover` and `sync`.

Public docs: [docs/](docs/) | SQL reference: [sql-docs.md](sql-docs.md) | Performance notes: [PERFORMANCE.md](PERFORMANCE.md) | AI agent guidance: [AGENTS.md](AGENTS.md)

## Repository structure

```
src/
  main.rs               Entry point, CLI dispatch, sync orchestration
  cli/                  Clap command definitions (discover, sync, query, schema, sql-help, benchmark)
  plugins/              Input/output plugins (s3_input, file_input, athena, file_output)
  buffer/               WAL segments, ingest buffer, compactor service
  discover/             Schema inference and type detection
  sqlrt/                DataFusion SQL runtime, parser, query execution, docs
  ingest_work.rs        Core ingestion loop and schema preparation
  ingest/               Ingestion internals (partitioning, record processing)
  serdes/               Serialization/deserialization (JSON, CSV, Parquet)
  converters/           Type converters (Arrow, Hive)
  helpers/              Configuration, S3 helpers, offsets DB, manifest
  metrics/              Stats collection and reporting
  adapters/             Storage adapters (S3, local)
tests/                  Integration tests
docs/                   MkDocs public documentation site
ci-e2e/                 E2E test orchestration scripts
soda/                   Soda Core data quality checks for E2E tests
test-data/              Sample data for tests
```

## Prerequisites

- Rust (version pinned in `rust-toolchain.toml`)
- `protoc` (protobuf compiler) — required by Arrow/Lance crates at compile time
- `libssl-dev` — required by `openssl-sys`

## Build and test

```bash
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy
```

## CLI commands

Run the product CLI during development:

```bash
cargo run -p skippr-cli --bin skippr -- <command> [flags]
```

Run the lightweight engine binary when you only need engine/runtime behavior:

```bash
cargo run --bin skipprd -- <command> [flags]
```

Both support `--config path/to/skippr.yml` for shared engine commands.

### discover

Connect to a source, sample data, infer schemas. Persists metadata to S3.

```bash
cargo run -p skippr-cli --bin skippr -- discover --pipeline bikehire --log
cargo run --bin skipprd -- --config skippr.yml discover --pipeline bikehire --log
```

Flags: `--pipeline/-p <name>`, `--verbose`

### sync

Run the ingestion loop: read source → WAL → compact → Parquet → S3 + Glue.

```bash
cargo run -p skippr-cli --bin skippr -- sync --pipeline bikehire --log
cargo run --bin skipprd -- --config skippr.yml sync --pipeline bikehire --log
```

Flags: `--pipeline/-p <name>`

### query

SQL engine (DataFusion) over Athena tables and WAL. Supports pipeline management commands.

```bash
cargo run --bin skipprd -- query --sql "SELECT COUNT(*) FROM bikehire"
cargo run --bin skipprd -- query --sql "ENABLE PIPELINE bikehire"
cargo run --bin skipprd -- query --sql "STREAM * FROM bikehire LIMIT 10"
```

Flags: `--sql/-s "<SQL>"`, `--watch <seconds>`, `--plain`

### schema

Inspect a pipeline's discovered schema.

```bash
cargo run --bin skipprd -- schema --pipeline bikehire
```

### sql-help

List all SQL extensions or export docs.

```bash
cargo run --bin skipprd -- sql-help
cargo run --bin skipprd -- sql-help --command "RESET PIPELINE" --output docs.md --format md
```

### benchmark

Generate synthetic data and measure throughput.

```bash
cargo run --bin skipprd -- benchmark -f 100 -r 50000 -s 800 --name baseline
```

## Configuration

Skippr is configured primarily via `skippr.yml`. Environment variables remain useful for secrets and deployment overrides.

```yaml
skippr:
  workspace: dev

pipelines:
  bikehire:
    data_source: data_sources.source
    data_sink: data_sinks.landing
    schema_sink: schema_sinks.glue

data_sources:
  source:
    S3:
      s3_bucket: skippr-public-sample-data
      s3_prefix: bike-hire

data_sinks:
  landing:
    Athena:
      s3_bucket: your-output-bucket
      s3_prefix: data/bikehire
      glue_database_name: skippr_quickstart
      athena_workgroup_name: primary
      athena_results_s3_bucket: your-athena-results

schema_sinks:
  glue:
    Glue:
      glue_database_name: skippr_quickstart
```

`data_sinks` are ingest/write targets. `skippr model` and `skippr query` use the same sink object. There is no `warehouses:` section.

See [the docs](docs/docs/configuration/skippr-yml.md) for the full config reference.

Legacy and deployment environment variables include:

### Pipeline identity

| Variable | Default | Description |
|---|---|---|
| `PIPELINE_NAME` | `default` | Pipeline name |
| `WORKSPACE_NAME` | `default` | Workspace/domain |
| `TENANT` | `default` | Tenant identifier |
| `SKIPPR_S3_BUCKET` | | State bucket (metadata, WAL, deadletters) |

### Input source

| Variable | Default | Description |
|---|---|---|
| `DATA_SOURCE_PLUGIN_NAME` | *(required)* | `s3` or `file` |
| `DATA_SOURCE_S3_BUCKET` | | Source S3 bucket |
| `DATA_SOURCE_S3_PREFIX` | | Source key prefix |
| `DATA_SOURCE_PATH` | | Local file path (for `file` plugin) |
| `DATA_SOURCE_BATCH_SIZE_BYTES` | `1024000` | Batch size in bytes |
| `DATA_SOURCE_BATCH_SIZE_SECONDS` | `600` | Batch time limit |

### Output destination

| Variable | Default | Description |
|---|---|---|
| `DATA_OUTPUT_S3_BUCKET` | *(required)* | Destination S3 bucket for Parquet |
| `DATA_OUTPUT_S3_PREFIX` | | Destination key prefix |
| `SCHEMA_OUTPUT_GLUE_DATABASE_NAME` | *(required)* | Glue catalog database |
| `DATA_OUTPUT_ATHENA_WORKGROUP_NAME` | | Athena workgroup |
| `DATA_OUTPUT_MAX_ASYNC_UPLOADS` | `16` | Concurrent Parquet uploads |

### Transforms

| Variable | Default | Description |
|---|---|---|
| `TRANSFORM_NAMESPACE_FIELDS` | | Fields for event type namespacing |
| `TRANSFORM_BATCH_PARTITION_FIELDS` | | Fields for Hive partitioning |
| `TRANSFORM_BATCH_TIME_FIELDS` | | Timestamp field(s) for time partitioning |
| `TRANSFORM_BATCH_TIME_UNIT` | | `year`, `month`, `day`, `hour`, `minute` |
| `TRANSFORM_FLATTEN_EVENTS` | `no` | Flatten nested structures |
| `TRANSFORM_BATCH_ORDER_FIELDS` | | Sort rows within Parquet files for predicate pruning |

### WAL and buffering

| Variable | Default | Description |
|---|---|---|
| `WAL_STORAGE` | `disk` | `disk`, `s3`, or `clustered` |
| `WAL_BYTES_PER_FILE` | auto | Optional WAL segment size override; defaults to `BUFFER_THRESHOLD_BYTES` clamped to 4-64 MiB |
| `WAL_MAX_DELAY_SECONDS` | `60` | Coarse max segment age before flush; runtime ACK latency is auto-tuned |
| `BUFFER_THRESHOLD_BYTES` | `10485760` | Buffer flush threshold (bytes) |
| `BUFFER_THRESHOLD_SECONDS` | `60` | Buffer flush threshold (time) |
| `DATA_DIR` | `./data` | Local dir for WAL + offsets DB |

### Operational

| Variable | Default | Description |
|---|---|---|
| `SKIPPR_CHAOS_MODE` | `no` | Random SIGKILL for testing exactly-once |
| `SCHEMA_AUTO_APPROVE` | `true` | Auto-approve schema changes |
| `RESET_OFFSETS` | `false` | Re-ingest from beginning |
| `RESET_METADATA` | `false` | Re-discover schema from scratch |

### JSON parsing

| Variable | Default | Description |
|---|---|---|
| `SKIPPR_ENABLE_SINGLE_QUOTE_PARSING` | `false` | Parse single-quoted JSON |
| `SKIPPR_ENABLE_UNICODE_PARSING` | `false` | Parse `u'string'` prefixed strings |

## Example: S3 to Athena

```bash
AWS_PROFILE=dev \
DATA_SOURCE_PLUGIN_NAME=s3 \
DATA_SOURCE_S3_BUCKET=my-source-bucket \
DATA_SOURCE_S3_PREFIX=events/ \
DATA_OUTPUT_S3_BUCKET=my-output-bucket \
DATA_OUTPUT_S3_PREFIX=warehouse/events \
SCHEMA_OUTPUT_GLUE_DATABASE_NAME=my_database \
PIPELINE_NAME=events \
SKIPPR_S3_BUCKET=my-state-bucket \
cargo run --bin skipprd -- sync --pipeline events --log
```

## Data type detection

Skippr automatically infers types during schema discovery:

- **Timestamps:** integer values are promoted to Timestamp (10-11 digits, epoch seconds) or TimestampMilli (13 digits, epoch millis). Only values after 2010-01-01 are recognised.
- **Numeric:** Integer, Long (large integers), Double
- **Other:** String, Boolean, Date (RFC 3339 strings)

## E2E tests

E2E tests run as GitHub Actions using the scripts in `ci-e2e/` and Soda checks in `soda/`. Key test pipelines:

| Test | What it validates |
|---|---|
| `bike_hire` | Basic S3 → Athena ingest |
| `bike_hire_many` | Multi-run chaos mode (local disk WAL) |
| `bike_hire_s3_wal_many` | Multi-run chaos mode (S3 WAL) |
| `event_types` | Namespace separation |
| `deadletters` | Dead letter capture and queryability |
| `offsets_db_rollup` | Offset database compaction |

## Roadmap

### Ingestion
- [x] Deadletter queues (capture, store, SQL query access)
- [ ] Deadletter replay
- [ ] Configurable retention and alerting
- [ ] S3-backed offset database (replace sled)
- [x] TigerBeetle-style deterministic WAL segment commits
- [x] S3-backed WAL segments
- [x] Structured logging via `tracing`

### Data modeling and querying
- [ ] External datasets (zero-copy, discover and query without ingestion)
- [ ] Datasets abstraction (internal + external, central catalog)
- [ ] Views and virtual views (non-destructive query-backed transforms)
- [ ] Field-level ABAC (attribute-based access control)
- [ ] End-to-end data lineage
- [ ] Source/dataset derivation lineage

### Outputs
- [ ] Apache Iceberg tables (replace Hive format)
