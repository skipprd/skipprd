<p align="center">
  <a href="https://skippr.io"><img src="https://skippr.io/logo-dark.png" alt="Skippr" width="200"></a>
</p>

<h3 align="center">Like Codex, but for Data.</h3>

<p align="center">
  An AI data agent that extracts, loads, cleanses, and models your data — producing<br>
  production-ready dbt models in your warehouse. One command. Minutes, not months.
</p>

<p align="center">
  <a href="https://skippr.io">Website</a> &middot;
  <a href="https://docs.skippr.io">Documentation</a> &middot;
  <a href="https://docs.skippr.io/getting-started/install">Install</a> &middot;
  <a href="https://docs.skippr.io/getting-started/quickstart">Quick Start</a> &middot;
  <a href="https://skippr.io/pricing">Pricing</a> &middot;
  <a href="https://skippr.io/contact-us">Contact</a>
</p>

<p align="center">
  <a href="https://github.com/skipprd/skipprd/releases"><img src="https://img.shields.io/github/v/release/skipprd/skipprd?label=release&color=03a0dc" alt="Latest Release"></a>
  <a href="https://twitter.com/skipprdata"><img src="https://img.shields.io/twitter/follow/skipprdata?style=social" alt="Follow on X"></a>
  <a href="https://www.linkedin.com/company/skippr-io/about/"><img src="https://img.shields.io/badge/LinkedIn-Skippr-0077B5?logo=linkedin" alt="LinkedIn"></a>
  <a href="https://skipprgroup.slack.com/ssb/redirect"><img src="https://img.shields.io/badge/Slack-Community-4A154B?logo=slack" alt="Slack"></a>
</p>

---

## What is Skippr?

Skippr is a CLI-based AI data agent. You point it at a source database and a destination warehouse, and it autonomously:

1. **Discovers** your source schemas — table names, column names, data types
2. **Extracts and loads** data into bronze tables in your warehouse
3. **Generates dbt models** — silver (staging) and gold (mart) layers — using AI-assisted schema mapping
4. **Validates and materialises** the models by running `dbt compile` and `dbt run`

The output is a **standard dbt project** that you own, review, extend, and plug into your existing CI/CD. Nothing proprietary.

```
skippr init my-project
skippr connect warehouse snowflake
skippr connect source mssql
skippr run
```

Four commands. That's extract, load, and a full bronze/silver/gold dbt project — compiled, validated, and materialised in your warehouse.

> **[Full install guide →](https://docs.skippr.io/getting-started/install)**

---

## Install

### macOS / Linux

```bash
curl -fsSL https://install.skippr.io | sh
```

### Windows (PowerShell)

PowerShell is the default terminal in VS Code on Windows.

```powershell
irm https://skippr.io/install.ps1 | iex
```

This installs `skippr` to your user PATH — run `skippr` from any terminal without needing `.\skippr.exe`.

<details>
<summary>cmd.exe users</summary>

```cmd
powershell -c "irm https://skippr.io/install.ps1 | iex"
```

</details>

### Manual download

Download the binary for your platform from the [Releases](https://github.com/skipprd/skipprd/releases) page and place it on your `PATH`.

### Verify

```bash
skippr --version
```

### Prerequisites

| Dependency | Why |
|---|---|
| Python 3.10+ | Required by dbt |
| dbt-core + warehouse adapter | Model compilation and materialisation |
| OpenSSL (Windows only) | Snowflake key-pair auth — `winget install OpenSSL` |

```bash
python3 -m venv .venv && source .venv/bin/activate
pip install dbt-core dbt-snowflake   # or: dbt-bigquery, dbt-postgres, etc.
```

> **[Full install guide with Windows + dbt adapter matrix →](https://docs.skippr.io/getting-started/install)**

---

## Quick Start

```bash
# 1. Log in (or create a new account — same command)
skippr user login

# 2. Create a project
mkdir my-workspace && cd my-workspace
skippr init mssql-migration

# 3. Connect your warehouse
skippr connect warehouse snowflake \
  --database ANALYTICS \
  --schema RAW \
  --warehouse COMPUTE_WH \
  --role ACCOUNTADMIN

# 4. Connect your source
skippr connect source mssql \
  --connection-string '${MSSQL_CONNECTION_STRING}'

# 5. Check everything is wired up
skippr doctor

# 6. Run the pipeline
skippr run
```

That's it. `skippr run` discovers your source schemas, extracts the data, loads it into Snowflake, and generates a complete dbt project with silver and gold models — compiled and materialised.

> **[Snowflake quick start →](https://docs.skippr.io/getting-started/quickstart-snowflake)** · **[BigQuery quick start →](https://docs.skippr.io/getting-started/quickstart-bigquery)**

---

## How It Works

When you run `skippr run`, the CLI orchestrates a multi-phase pipeline:

```
Your Source
  │
  │  discover — reads schemas and infers structure
  ▼
Schema Discovery
  │
  │  map — deterministic typed destination schemas
  ▼
Schema Mapping
  │
  │  sync — extracts rows, loads into warehouse
  ▼
Bronze Tables (raw data in your warehouse)
  │
  │  dbt — generates and runs silver/gold models
  ▼
Silver Models — staging: cleaned, typed, renamed
  │
  ▼
Gold Models — marts: business-ready aggregations
```

### What happens at each step

| Phase | What it does | Uses AI? |
|---|---|---|
| **Discover** | Reads source metadata: table names, column names, data types. No manual DDL needed. | No |
| **Map** | Designs destination schemas: clean names, type casts, staging structure. Only metadata used. | No |
| **Sync** | Extracts rows from source, writes directly to warehouse bronze schema. Data never leaves your machine. | No |
| **Model** | Generates a complete dbt project: source definitions, silver staging models, gold mart models. | Yes |
| **Validate** | Compiles and runs dbt against your warehouse. Iterates on failures automatically. | Yes |

Schema discovery and mapping are **deterministic algorithms** — consistent and backwards-compatible. AI assists only with dbt model generation, test generation, and catalog metadata.

> **[How it works →](https://docs.skippr.io/getting-started/how-it-works)** · **[Core concepts →](https://docs.skippr.io/advanced/core-concepts)**

---

## Architecture

### Medallion layers

Skippr organises data into three tiers inside your warehouse:

| Tier | Schema | Contents | Created by |
|---|---|---|---|
| **Bronze** | `RAW` (your configured schema) | Raw extracted data, exactly as it appeared in the source | `skippr` extract-and-load |
| **Silver** | `<project>_silver` | Cleaned, typed, and renamed staging models | dbt (AI-generated) |
| **Gold** | `<project>_gold` | Business-ready marts and aggregations | dbt (AI-generated) |

### Incremental by default

Re-running `skippr run` doesn't start from scratch:

- **Data sync** — offsets are tracked internally. Only new and changed rows are extracted.
- **dbt models** — existing models are preserved. The agent updates or adds models as the source evolves.
- **Exactly-once delivery** — extraction offsets are committed atomically with the load step. Interrupted runs don't produce duplicates.

### dbt output

Skippr generates a standard, fully functional dbt project:

```
models/
├── schema.yml                   # source definitions
└── staging/
    ├── stg_raw_customers.sql    # silver model
    └── stg_raw_orders.sql       # silver model
```

Plus `dbt_project.yml`, `profiles.yml`, and `packages.yml` — all auto-configured for your warehouse. The project is yours: add tests, snapshots, custom gold models, or plug it into your existing dbt CI/CD.

---

## Data Privacy

Row-level data only ever exists in **two places**: the machine running `skippr`, and your warehouse.

| What | Where it goes |
|---|---|
| **Source data** | Read locally, written directly to the warehouse API. Never sent to Skippr or any third party. |
| **AI modeling** | Uses only metadata (table names, column names, types) by default. Data samples are opt-in. |
| **Skippr backend** | Receives only pipeline metadata and usage metrics (run status, table counts, credits). No source or warehouse data. |
| **Credentials** | Live in environment variables. Never stored in config files. |

---

## Supported Connectors

### Sources

| Category | Connectors |
|---|---|
| **Databases** | MSSQL, MySQL, PostgreSQL, Redshift, MongoDB, DynamoDB, ClickHouse, MotherDuck |
| **Object Stores** | S3, SFTP, Delta Lake |
| **Streaming** | Kafka, SQS, Kinesis, AMQP (RabbitMQ), SNS, EventBridge, MQTT, WebSocket |
| **HTTP** | HTTP Client, HTTP Server |
| **Other** | Socket (TCP/UDP/Unix), StatsD, Local File, Stdin |

### Destinations (Warehouses)

| Warehouse | dbt adapter | Docs |
|---|---|---|
| Snowflake | `dbt-snowflake` | [Connector guide](https://docs.skippr.io/connectors/destinations/snowflake) |
| Google BigQuery | `dbt-bigquery` | [Connector guide](https://docs.skippr.io/connectors/destinations/bigquery) |
| PostgreSQL | `dbt-postgres` | [Connector guide](https://docs.skippr.io/connectors/destinations/postgres) |
| AWS Athena | `dbt-athena-community` | [Connector guide](https://docs.skippr.io/connectors/destinations/athena) |
| Databricks | `dbt-databricks` | [Connector guide](https://docs.skippr.io/connectors/destinations/databricks) |
| Azure Synapse | `dbt-synapse` | [Connector guide](https://docs.skippr.io/connectors/destinations/synapse) |
| Amazon Redshift | `dbt-redshift` | [Connector guide](https://docs.skippr.io/connectors/destinations/redshift) |
| ClickHouse | `dbt-clickhouse` | [Connector guide](https://docs.skippr.io/connectors/destinations/clickhouse) |
| MotherDuck | `dbt-duckdb` | [Connector guide](https://docs.skippr.io/connectors/destinations/motherduck) |

Plus cloud storage destinations (GCS, Azure Blob, SFTP) and messaging (AMQP).

> **[All source connectors →](https://docs.skippr.io/connectors/sources/)** · **[All destination connectors →](https://docs.skippr.io/connectors/destinations/)**

---

## Authentication

**Interactive (local development):**

```bash
skippr user login
```

**CI/CD (API key):**

```bash
export SKIPPR_API_KEY="sk_live_..."
skippr run
```

Create and manage API keys:

```bash
skippr user create-api-key --name "github-actions"
skippr user list-api-keys
skippr user revoke-api-key --key-id <id>
```

> **[Authentication guide →](https://docs.skippr.io/getting-started/authentication)**

---

## Configuration

Skippr uses a single `skippr.yaml` file in your project root:

```yaml
project: mssql_migration

warehouse:
  kind: snowflake
  database: ANALYTICS
  schema: RAW
  warehouse: COMPUTE_WH
  role: ACCOUNTADMIN

source:
  kind: mssql
  connection_string: ${MSSQL_CONNECTION_STRING}
```

Credentials are always environment variables — never stored in config files.

> **[Config file reference →](https://docs.skippr.io/configuration/config-file)** · **[Environment variables →](https://docs.skippr.io/configuration/environment-variables)**

---

## CLI Reference

| Command | Description | Docs |
|---|---|---|
| `skippr init <project>` | Create a new project | [init →](https://docs.skippr.io/cli/init) |
| `skippr connect warehouse <kind>` | Configure warehouse connection | [connect →](https://docs.skippr.io/cli/connect/) |
| `skippr connect source <kind>` | Configure source connection | [connect →](https://docs.skippr.io/cli/connect/) |
| `skippr doctor` | Verify binaries, credentials, and config | [doctor →](https://docs.skippr.io/cli/doctor) |
| `skippr run` | Execute the full pipeline | [run →](https://docs.skippr.io/cli/run) |
| `skippr user login` | Authenticate interactively | [user →](https://docs.skippr.io/cli/user) |
| `skippr user buy-credits` | Add balance to your account | [user →](https://docs.skippr.io/cli/user) |

---

## Pricing

Skippr uses credit-based pricing. Start free with 100 credits.

| Action | Credits | Cost |
|---|---|---|
| Table sync | 5 | $0.50 |
| Field discovered | 0.2 | $0.02 |
| Pipeline run | 3 | $0.30 |
| LLM input tokens (per 1K) | 0.5 | $0.05 |
| LLM output tokens (per 1K) | 2 | $0.20 |

Credits never expire. No subscriptions required on the free tier.

> **[Full pricing + estimator →](https://skippr.io/pricing)**

---

## Troubleshooting

| Issue | Fix | Docs |
|---|---|---|
| `skippr: command not found` | Re-run the install script or add the binary to your PATH | [Install →](https://docs.skippr.io/getting-started/install) |
| `dbt: command not found` | Activate your Python venv: `source .venv/bin/activate` | [Install →](https://docs.skippr.io/getting-started/install) |
| `openssl: command not found` (Windows) | `winget install OpenSSL` and restart your terminal | [Install →](https://docs.skippr.io/getting-started/install) |
| Snowflake `390197 — MFA required` | Switch to key-pair auth | [Snowflake connector →](https://docs.skippr.io/connectors/destinations/snowflake) |
| Snowflake `250001 — connection failed` | Check `SNOWFLAKE_ACCOUNT` format (`MYORG-MYACCOUNT`) | [Snowflake connector →](https://docs.skippr.io/connectors/destinations/snowflake) |
| `Insufficient privileges` | Verify role grants | [Snowflake connector →](https://docs.skippr.io/connectors/destinations/snowflake#required-grants) |

> **[Full troubleshooting guide →](https://docs.skippr.io/operations/troubleshooting)** · **[Logs and artifacts →](https://docs.skippr.io/operations/logs)**

---

## Links

| | |
|---|---|
| **Website** | [skippr.io](https://skippr.io) |
| **Documentation** | [docs.skippr.io](https://docs.skippr.io) |
| **Releases** | [GitHub Releases](https://github.com/skipprd/skipprd/releases) |
| **Slack** | [skipprgroup.slack.com](https://skipprgroup.slack.com/ssb/redirect) |
| **Twitter / X** | [@skipprdata](https://twitter.com/skipprdata) |
| **LinkedIn** | [Skippr](https://www.linkedin.com/company/skippr-io/about/) |
| **Contact** | [skippr.io/contact-us](https://skippr.io/contact-us) |

---

<p align="center">
  <sub>Runs on your machine. Your data stays local.</sub><br>
  <sub>© Skippr</sub>
</p>
