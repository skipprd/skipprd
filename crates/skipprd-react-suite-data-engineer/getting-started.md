# Getting Started: MSSQL to Snowflake

This guide walks you through **extracting data from MSSQL**, **loading it into Snowflake** as a bronze (raw) tier, and then **building silver/gold dbt models** on top — all driven by `skippr`.

What you'll end up with:

- Raw MSSQL tables landed in Snowflake (`ANALYTICS.RAW`)
- AI-generated silver dbt models (`mssql_migration_silver`)
- Gold-tier models ready for downstream use (`mssql_migration_gold`)

---

## Prerequisites

### skippr binary

Download `skippr` for your platform and place it on your PATH:

```bash
skippr --version
```

### Python 3.10+ and dbt

Python is required to run `dbt`, which `skippr` uses for model compilation and materialisation.

| Tool | Why | Install |
|------|-----|---------|
| **Python 3.10+** | Hosts `dbt` | [python.org](https://www.python.org/) |

Verify:

```bash
python3 --version
```

### Snowflake account

You will need:

| Item | Example |
|------|---------|
| Account identifier | `RSSKNWT-KC53195` or `xy12345.us-east-1` |
| Username | service account or personal login |
| RSA key pair **or** password | Key-pair auth is required when MFA is enabled (most accounts) |
| Warehouse | `COMPUTE_WH` |
| Role | `TRANSFORMER` (or any role with read on raw + create on target schemas) |
| Database | `ANALYTICS` |
| Raw schema (bronze) | `RAW` — where MSSQL data will land |

### Generate an RSA key pair for Snowflake

Snowflake accounts with MFA enabled (the default for most orgs) **cannot** use password auth for headless/programmatic tools. Key-pair authentication bypasses MFA entirely and is the recommended approach.

**1. Generate an unencrypted PKCS#8 private key:**

```bash
openssl genrsa 2048 | openssl pkcs8 -topk8 -inform PEM -out snowflake_key.p8 -nocrypt
```

This produces `snowflake_key.p8` — your private key. Keep it safe and never commit it to version control.

**2. Extract the public key:**

```bash
openssl rsa -in snowflake_key.p8 -pubout -out snowflake_key.pub
```

**3. Assign the public key to your Snowflake user:**

Log into Snowsight (or SnowSQL) and run:

```sql
ALTER USER YOURUSERNAME SET RSA_PUBLIC_KEY='MIIBIjANBgkqh...';
```

Copy the contents of `snowflake_key.pub` and paste **only** the base64 body — strip the `-----BEGIN PUBLIC KEY-----` and `-----END PUBLIC KEY-----` lines.

**4. Verify the key was assigned:**

```sql
DESC USER YOURUSERNAME;
-- Look for RSA_PUBLIC_KEY_FP — it should show a fingerprint like SHA256:...
```

You will reference the private key file path in the environment variables below.

---

## 1. Provide an MSSQL source

`skippr` extracts data from MSSQL and loads it into Snowflake automatically. You just need a running MSSQL instance with the tables you want to migrate.

### Option A — Use an existing MSSQL server

If you already have a SQL Server instance, note the connection string. It follows the ADO.NET format:

```
server=tcp:YOUR_HOST,1433;database=YOUR_DB;user id=sa;password=YOUR_PASS;TrustServerCertificate=true
```

### Option B — Spin up a local MSSQL with Docker (POC / dev)

```bash
docker compose -f test/el-integration/docker-compose.yml up -d
```

Wait for the seed service to complete (check with `docker compose logs seed`), then use:

```
server=tcp:127.0.0.1,1433;database=testdb;user id=sa;password=Skippr!Test123;TrustServerCertificate=true
```

The seed creates three tables: `dbo.customers`, `dbo.orders`, and `dbo.order_items`.

To tear down later:

```bash
docker compose -f test/el-integration/docker-compose.yml down -v
```

---

## 2. Set up the dbt environment

`skippr` uses `dbt` for model compilation, validation, and materialisation. You need `dbt-core` and the Snowflake adapter installed in a Python virtual environment.

### Create a virtual environment

```bash
mkdir -p skippr-workspace && cd skippr-workspace

python3 -m venv .venv
source .venv/bin/activate
pip install --upgrade pip
```

### Install dbt with the Snowflake adapter

```bash
pip install dbt-core dbt-snowflake
```

Confirm both are installed:

```bash
dbt --version
```

You should see output listing `dbt-core` and `dbt-snowflake` with their versions.

**Important:** The virtual environment must be activated whenever you run `skippr`, so that `dbt` is available on PATH.

---

## 3. Authenticate with Skippr

`skippr` requires an authenticated session for cloud storage, server-provided LLM access, and usage metering.

### Interactive login (recommended)

```bash
skippr user login
```

This sends a one-time code to your email. Enter it when prompted and your session credentials are stored locally.

### CI / non-interactive

For headless environments, set an API key instead:

```bash
export SKIPPR_API_KEY="sk-..."
```

You can generate API keys from an authenticated session with `skippr user create-api-key`.

---

## 4. Set environment variables

### Required

```bash
export SNOWFLAKE_ACCOUNT="RSSKNWT-KC12345"
export SNOWFLAKE_USER="YOURUSERNAME"

export MSSQL_CONNECTION_STRING="server=tcp:127.0.0.1,1433;database=testdb;user id=sa;password=Skippr!Test123;TrustServerCertificate=true"
```

### Optional

```bash
export LLM_API_KEY="sk-..."   # Optional — the server provides a key after authentication
```

### Snowflake authentication

**Key-pair auth (recommended — required when MFA is enabled):**

```bash
export SNOWFLAKE_PRIVATE_KEY_PATH="/path/to/snowflake_key.p8"
```

See [Generate an RSA key pair for Snowflake](#generate-an-rsa-key-pair-for-snowflake) above for how to create the key.

**Password auth (only if MFA is not enforced on the account):**

```bash
export SNOWFLAKE_PASSWORD="your_password"
```

Credentials are read from the environment at runtime and are **not** written to disk in plaintext.

---

## 5. Initialise the project

```bash
skippr init mssql-migration
```

This creates `skippr.yaml` with your project name and a `.env.example` showing the required variables.

---

## 6. Connect the warehouse and source

### Connect Snowflake

```bash
skippr connect warehouse snowflake \
  --database ANALYTICS \
  --schema RAW \
  --warehouse COMPUTE_WH \
  --role ACCOUNTADMIN
```

Or run without flags to be prompted interactively:

```bash
skippr connect warehouse snowflake
```

### Connect MSSQL

```bash
skippr connect source mssql \
  --connection-string '${MSSQL_CONNECTION_STRING}'
```

Using `${MSSQL_CONNECTION_STRING}` tells `skippr` to read the value from your environment at runtime, keeping secrets out of the config file.

---

## 7. Check prerequisites

```bash
skippr doctor
```

This verifies that all binaries, credentials, and config are in place. Fix anything marked `[FAIL]` before proceeding.

---

## 8. Run the pipeline

Make sure your virtual environment is activated, then:

```bash
skippr run
```

### Log modes

By default, `skippr run` renders a **live terminal UI** showing phases, tasks, and progress in real time.

| Flag | Behavior |
|------|----------|
| _(none)_ | Terminal UI enabled. |
| `--log info` | Disables the TUI; prints plain structured logs to stdout. Recommended for CI or piped output. |
| `--log debug` | Debug-level logs. |
| `--log trace` | Most verbose. |

If you see `"terminal mode not enabled: stdout is not a TTY"`, use `--log info` instead.

**Example (headless / CI-friendly):**

```bash
skippr run --log info
```

### What happens when you run

1. **Discover** — reads source schemas from MSSQL.
2. **Sync** — extracts rows from MSSQL and loads them into Snowflake bronze tables.
3. **Verify** — confirms the destination tables exist and are queryable.
4. **Plan** — designs a silver (staging) layer with one `stg_*` model per raw table.
5. **Author** — writes dbt SQL models with source references, type casting, and column renaming.
6. **Validate** — runs `dbt compile` / `dbt run` against Snowflake.
7. **Review** — checks the generated models for quality.

---

## 9. Verify outputs

### Your config file

After `init` and `connect`, `skippr.yaml` looks like this:

```yaml
project: mssql-migration

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

That's the entire config. Everything else is handled automatically.

### Local artifacts

All pipeline artifacts are stored under `.skippr/` in your working directory:

```
.skippr/
└── local/
    └── dev/
        └── mssql-migration/
            ├── logs/             # Per-run log files
            └── pipeline/         # Generated pipeline config
```

### dbt project files

The generated dbt models land in the working directory:

```
models/
├── schema.yml                     # Source definitions (pointing at RAW tables)
└── staging/
    ├── stg_raw_customers.sql      # Silver model for customers
    └── stg_raw_orders.sql         # Silver model for orders
```

Each staging model uses `{{ source("raw", "customers") }}` to reference the bronze table and applies cleansing (casting, renaming, null handling).

### Run dbt manually (optional sanity check)

After the pipeline finishes, you can run dbt directly against the generated project:

```bash
source .venv/bin/activate
dbt debug   --profiles-dir .   # Verify connection
dbt run     --profiles-dir .   # Materialize models into Snowflake
dbt test    --profiles-dir .   # Run any generated tests
```

Verify in Snowflake:

```sql
USE DATABASE ANALYTICS;
SHOW SCHEMAS LIKE '%mssql_migration%';
-- Expect: mssql_migration_silver, mssql_migration_gold

SELECT * FROM mssql_migration_silver.stg_raw_customers LIMIT 10;
```

---

## Troubleshooting

### MFA error: `390197 — Multi-factor authentication is required`

This means your Snowflake account enforces MFA, so password auth cannot work for headless tools. Switch to key-pair authentication:

1. Generate an RSA key pair — see [Generate an RSA key pair for Snowflake](#generate-an-rsa-key-pair-for-snowflake)
2. Replace `SNOWFLAKE_PASSWORD` with `SNOWFLAKE_PRIVATE_KEY_PATH` pointing to your `.p8` file
3. Re-run `skippr run`

### Snowflake connection errors

| Symptom | Fix |
|---------|-----|
| `Failed to connect: 250001` | Verify `SNOWFLAKE_ACCOUNT` format — use the org-account form (e.g. `MYORG-MYACCOUNT`) or include the region (e.g. `xy12345.us-east-1`) |
| `Incorrect username or password` | Check `SNOWFLAKE_USER` and `SNOWFLAKE_PASSWORD` / `SNOWFLAKE_PRIVATE_KEY_PATH` env vars |
| `Insufficient privileges` | Ensure the role has USAGE on the warehouse, database, and raw schema, plus CREATE SCHEMA on the database for silver/gold |

### `dbt: command not found` or `dbt-snowflake` adapter missing

Activate your Python virtual environment and confirm the adapter is installed:

```bash
source .venv/bin/activate
dbt --version
```

If `dbt-snowflake` is not listed, reinstall:

```bash
pip install dbt-core dbt-snowflake
```

### LLM errors (401 / timeouts)

- If you are authenticated (`skippr user login`), the server provides an LLM key automatically. A custom `LLM_API_KEY` is only needed if you want to use your own key.
- If requests timeout, set `LLM_HTTP_TIMEOUT_SECS=120` in the environment.

---

## Quick reference

| What | Where |
|------|-------|
| Config file | `skippr.yaml` (working directory) |
| Local artifacts | `.skippr/local/dev/mssql-migration/` |
| dbt models | `models/staging/stg_*.sql` |
| Source definitions | `models/schema.yml` |
| Run logs | `.skippr/local/dev/mssql-migration/logs/` |
| Snowflake raw schema | `ANALYTICS.RAW` |
| Snowflake silver schema | `ANALYTICS.MSSQL_MIGRATION_SILVER` |
| Snowflake gold schema | `ANALYTICS.MSSQL_MIGRATION_GOLD` |
