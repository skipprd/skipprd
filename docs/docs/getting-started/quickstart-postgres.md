---
title: "Quickstart: PostgreSQL"
description: Load sample JSON from S3 into a PostgreSQL table you can run locally with Docker, using skippr.yml, skipprd discover, and skipprd sync.
---

# Quickstart: PostgreSQL

In about ten minutes you will load public sample JSON from S3 into a PostgreSQL table and query it with `psql`. You can run PostgreSQL locally in Docker, so you do not need a cloud warehouse account.

Skipprd infers the schema, creates the PostgreSQL schema and table for you, and inserts the rows.

## Before you begin

You need:

- **Skipprd installed.** Check with `skipprd --version`, or `python -c "import skippr"` if you use Python. See [Install](/getting-started/install).
- **PostgreSQL** reachable from the machine running Skipprd. This guide starts one with **Docker**; check with `docker --version`. To use an existing server instead, you need a database and a user that can create a schema and tables in it.
- **AWS credentials** that can read `s3://skippr-public-sample-data/bike-hire/`. Any AWS account works.

Export your credentials and a password for PostgreSQL, then create an empty working directory. Skipprd reads `skippr.yml` from the current directory, so run every command from there.

```bash
export AWS_ACCESS_KEY_ID="your-access-key-id"
export AWS_SECRET_ACCESS_KEY="your-secret-access-key"
export AWS_DEFAULT_REGION="us-east-1"
export POSTGRES_PASSWORD="choose-a-password"

mkdir skippr-postgres && cd skippr-postgres
```

## 1. Start PostgreSQL

Skip this step if you already have a server.

```bash
docker run --name skippr-postgres -d -p 5432:5432 \
  -e POSTGRES_PASSWORD="$POSTGRES_PASSWORD" \
  -e POSTGRES_DB=analytics \
  postgres:16
```

This starts PostgreSQL on `localhost:5432` with a database named `analytics` and the user `postgres`.

## 2. Describe the pipeline

Each tab writes the same `skippr.yml`.

::: code-group

```python [Python]
from skippr import Config, DataSinkPostgres, DataSourceS3, EnvRef, LocalStorage, Pipeline

cfg = Config().workspace("quickstart").storage(LocalStorage())
sample = cfg.data_source(
    "sample",
    DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire"),
)
warehouse = cfg.data_sink(
    "warehouse",
    DataSinkPostgres(
        host="localhost",
        port=5432,
        user="postgres",
        password=EnvRef("POSTGRES_PASSWORD"),
        database="analytics",
        schema="public",
        sslmode="disable",
    ),
)
cfg.pipeline("bikehire", Pipeline(data_source=sample, data_sink=warehouse))
cfg.save("skippr.yml")
```

```bash [CLI]
skipprd --workspace quickstart --storage-mode local connect data-source s3 \
  --pipeline bikehire \
  --name sample \
  --s3-bucket skippr-public-sample-data \
  --s3-prefix bike-hire

skipprd connect data-sink postgres \
  --pipeline bikehire \
  --name warehouse \
  --host localhost \
  --port 5432 \
  --user postgres \
  --password '${POSTGRES_PASSWORD}' \
  --database analytics \
  --schema public \
  --sslmode disable
```

```yaml [YAML]
skippr:
  workspace: quickstart
  skipprd_el_storage_mode: local

pipelines:
  bikehire:
    data_source: data_sources.sample
    data_sink: data_sinks.warehouse

data_sources:
  sample:
    S3:
      s3_bucket: skippr-public-sample-data
      s3_prefix: bike-hire

data_sinks:
  warehouse:
    Postgres:
      host: localhost
      port: 5432
      user: postgres
      password: ${POSTGRES_PASSWORD}
      database: analytics
      schema: public
      sslmode: disable
```

:::

What the settings mean:

- `password` must be an environment reference. In Python, `EnvRef("POSTGRES_PASSWORD")` writes `password: ${POSTGRES_PASSWORD}`; in the CLI, single quotes stop your shell from expanding it. Skipprd reads the value when the pipeline starts, so the password never lands in `skippr.yml`. You can also put it in a `.env` file next to `skippr.yml`.
- `sslmode: disable` suits the local Docker container, which has no TLS. For a remote server, leave `sslmode` out: the default, `prefer`, uses TLS when the server offers it.
- `host`, `port`, and `schema` default to `localhost`, `5432`, and `public`. They are spelled out here so you can see what to change for your own server.
- `LocalStorage()` (`skipprd_el_storage_mode: local`) keeps Skipprd's own state, such as the discovered schema and how far it has read, in `./data`. That is fine for a trial; for production, see [State store](/configuration/skippr-store).

## 3. Discover the schema

::: code-group

```python [Python]
import skippr

s = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
s.discover()
```

```bash [CLI]
skipprd discover --pipeline bikehire
skipprd schema --pipeline bikehire
```

:::

`discover` samples the JSON and records each field's name and type. It does not touch PostgreSQL. `skipprd schema` prints the fields, which become your table's columns.

## 4. Sync

::: code-group

```python [Python]
import skippr

s = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
s.sync(once=True)
```

```bash [CLI]
skipprd sync --pipeline bikehire --once
```

:::

Sync reads the source and writes each batch to Skipprd's write-ahead log (WAL) first. A crash retries that committed batch; PostgreSQL applies it once. It then creates the schema and the `bikehire` table if they do not exist and inserts the rows. When new fields appear later, Skipprd adds the columns before inserting.

`--once` (`once=True`) runs a single pass and exits. Without it, sync keeps running and checks the source for new data on a schedule.

## Check it worked

Count the rows and look at the table definition:

```bash
docker exec -it skippr-postgres psql -U postgres -d analytics \
  -c 'SELECT count(*) AS row_count FROM public.bikehire;' \
  -c '\d public.bikehire'
```

You should see a non-zero `row_count`, and columns that match the fields `skipprd schema` printed in step 3. The table is named after the pipeline. Column names are lower case.

If you use your own server, run the same `SELECT` with `psql` or any SQL client.

## Troubleshooting

- **`connection refused`** — PostgreSQL is not listening on `host`:`port`. Check the container is running with `docker ps`, or check the host and port of your server.
- **`password authentication failed`** — `POSTGRES_PASSWORD` in the shell running Skipprd does not match the database user's password. If you changed it after starting the container, recreate the container or update the user's password.
- **`database "analytics" does not exist`** — Skipprd creates schemas and tables, not databases. Create the database first, or set `database` to one that exists.
- **`permission denied for database` or `for schema`** — the user cannot create the schema or tables. Grant `CREATE` on the database, or `USAGE` and `CREATE` on the schema.
- **A TLS or SSL error** — the server's TLS settings do not match `sslmode`. Use `sslmode: disable` for a local server without TLS.
- **`relation "public.bikehire" does not exist`** — sync has not written yet, or it failed. Rerun `skipprd sync --pipeline bikehire --once --log` and look for the error.
- **`AccessDenied` reading the sample bucket** — your AWS credentials are missing or lack `s3:GetObject` and `s3:ListBucket`. Run `aws sts get-caller-identity` to see which principal Skipprd uses.

For more, see [Troubleshooting](/operations/troubleshooting).

## Next steps

- [PostgreSQL destination](/connectors/outputs/postgres) — every option.
- [PostgreSQL source](/connectors/inputs/postgres) — extract from database tables instead of files.
- [Change data capture](/cdc/) — replicate inserts, updates, and deletes to current state.
- [Quickstart: Snowflake](/getting-started/quickstart-snowflake) — move to a cloud warehouse.
- [How Skipprd works](/concepts/how-it-works) — what happens between source and destination.
