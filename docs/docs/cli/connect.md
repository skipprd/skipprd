---
title: skipprd connect
description: Build skippr.yml from the command line by adding sources, destinations, and schema sinks, with secrets kept as ${ENV} references.
---

# skipprd connect

`skipprd connect` adds a source, destination, or schema sink to `skippr.yml` and wires it into a pipeline. Use it instead of hand-editing YAML, in setup scripts, or in CI. Each run merges into the existing file and checks the result before writing, so a typo never leaves you with a half-written config. Python `Config.save()` uses the same merge, so the CLI and Python produce identical files.

## Usage

::: code-group

```python [Python]
from skippr import Config, DataSourceS3, LocalStorage, Pipeline

cfg = Config.discover().workspace("bikehire").storage(LocalStorage())
src = cfg.data_source(
    "sample",
    DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire"),
)
cfg.pipeline("bikehire", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd [GLOBAL FLAGS] connect data-source <kind> --pipeline <pipeline> --name <entry> [--<field> <value> ...]
skipprd [GLOBAL FLAGS] connect data-sink   <kind> --pipeline <pipeline> --name <entry> [--<field> <value> ...]
skipprd [GLOBAL FLAGS] connect schema-sink <kind> --pipeline <pipeline> --name <entry> [--<field> <value> ...]
```

```yaml [YAML]
skippr:
  workspace: bikehire
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

:::

The three roles write to three sections of `skippr.yml`:

| Command | Writes the entry to | Links it from the pipeline as |
|---|---|---|
| `connect data-source` | `data_sources.<name>` | `pipelines.<pipeline>.data_source` |
| `connect data-sink` | `data_sinks.<name>` | `pipelines.<pipeline>.data_sink` |
| `connect schema-sink` | `schema_sinks.<name>` | `schema_sink` on the pipeline's data sink |

Run `skipprd connect data-source --help` (or `data-sink`, `schema-sink`) to list every kind, and `skipprd connect data-sink snowflake --help` to list a kind's flags.

## Options

These flags apply to every `connect` command.

| Flag | Default | Description |
|---|---|---|
| `--pipeline <NAME>` | Required | Pipeline to add the entry to. Created if it does not exist. On an interactive terminal Skipprd prompts for it if you leave it out. |
| `--name <NAME>` | Required | Entry name inside `data_sources`, `data_sinks`, or `schema_sinks`. Must be non-empty and contain no `.`. Prompted for on a terminal if missing. |
| `--config <PATH>` | `./skippr.yml` | File to write. Must end in `.yml` or `.yaml`. Without it, `connect` writes to the config file in the current directory, or creates `./skippr.yml`. |
| `--workspace <NAME>` | None | Sets `skippr.workspace`. |
| `--tenant <NAME>` | None | Sets `skippr.tenant`. |
| `--storage-mode <MODE>` | None | Sets `skippr.skipprd_el_storage_mode`: `local` keeps engine state on local disk, `s3` keeps it in `skippr.skippr_s3_bucket`. Setting `local` without `--skippr-s3-bucket` removes any existing `skippr_s3_bucket`. |
| `--skippr-s3-bucket <BUCKET>` | None | Sets `skippr.skippr_s3_bucket`, the bucket for engine state in `s3` storage mode. |
| `--wal-s3-bucket <BUCKET>` | None | Sets `skippr.wal_s3_bucket`, a dedicated bucket for WAL segments. |
| `--store-type <TYPE>` | None | Sets `skippr.store.type`: `sled`, `dynamodb`, or `cloud-tables`. Replaces the whole `store` block. |
| `--store-name <NAME>` | None | Sets `skippr.store.name`. Requires `--store-type`. |
| `--log [LEVEL]` | Off | Print logs to stderr. |

`--wal-storage` is not written to `skippr.yml`. Set it per run with the flag or the `WAL_STORAGE` environment variable.

### Connector fields

Every other flag is a connector field. The flag is the YAML key with underscores turned into hyphens, so `s3_bucket` is `--s3-bucket`. Nested keys join with hyphens: `object_store.type` is `--object-store-type` and `auth.strategy` is `--auth-strategy`. Each connector page lists every key; start at the [connector catalog](/connectors/).

| Value type | How to pass it |
|---|---|
| Text | `--region eu-west-2` |
| Number or true/false | `--port 5432`, `--object-store-path-style true`. Skipprd writes them to YAML as numbers and booleans. |
| List | Repeat the flag: `--streams customers --streams invoices` |
| Map or list of objects | A YAML or JSON string: `--headers '{Accept: application/json}'` |
| Secret | An environment reference in single quotes: `--password '${POSTGRES_PASSWORD}'` |

### Kind names

Kinds are the connector name in lowercase with hyphens. Most are what you expect; these are the destinations and schema sinks:

| Role | Kinds |
|---|---|
| `data-sink` | `amqp`, `athena`, `athena-iceberg`, `azure-blob`, `bigquery`, `clickhouse`, `databricks`, `duckdb`, `file`, `gcs`, `motherduck`, `postgres`, `redshift`, `s3`, `sftp`, `skippr-lake`, `snowflake`, `stdout`, `synapse` |
| `schema-sink` | `athena-iceberg`, `bigquery`, `clickhouse`, `duckdb`, `glue`, `motherduck`, `postgres`, `redshift`, `skippr-lake`, `snowflake` |

Source kinds with names you might not guess: `mssql` (SQL Server), `delta-lake`, `linked-in-ads`, `sum-up`, `ad-roll-ads`, `x-ads`, `google-analytics` (GA4).

## How the merge works

Each `connect` run reads the file, merges your change, validates the result, and only then replaces the file. If validation fails, nothing is written, including global flags such as `--workspace`.

- Each field you pass replaces that field. Fields you do not pass are kept.
- A nested flag replaces its whole block. Passing only `--object-store-type r2` on a later run replaces the entire `object_store` block, so pass every `object_store` field together.
- Other pipelines and entries are left alone.
- An entry keeps its connector kind. You cannot turn a `Postgres` entry into a `Snowflake` entry; choose a new `--name`.
- Secret fields must be exactly `${NAME}`. Plaintext secrets are refused, so they never land in `skippr.yml`.
- `connect schema-sink` needs a data sink on the pipeline first, because it links the schema sink to that sink.
- A pipeline you are still building one entry at a time is allowed. Full checks run when `discover`, `sync`, or `doctor` reads the file.

### Paired sinks

SkipprLake, AthenaIceberg, and DuckDB destinations share one configuration with their schema sink. When you add the schema sink with no fields, it starts as a copy of the data sink. Later field changes to either one are applied to both. If the two have already drifted apart, `connect` refuses to write until you make them match.

## Examples

### Add a destination with a secret

Keep the password in an environment variable. Single quotes stop your shell from expanding `${POSTGRES_PASSWORD}`, so the reference, not the value, is written.

```bash
export POSTGRES_PASSWORD='replace-me'
```

::: code-group

```python [Python]
from skippr import Config, DataSinkPostgres, EnvRef, Pipeline

cfg = Config.discover()
warehouse = cfg.data_sink(
    "warehouse",
    DataSinkPostgres(
        host="localhost",
        port=5432,
        user="skippr",
        password=EnvRef("POSTGRES_PASSWORD"),
        database="analytics",
    ),
)
cfg.pipeline(
    "bikehire",
    Pipeline(data_source=cfg.get_data_source("sample"), data_sink=warehouse),
)
cfg.save()
```

```bash [CLI]
skipprd connect data-sink postgres \
  --pipeline bikehire \
  --name warehouse \
  --host localhost \
  --port 5432 \
  --user skippr \
  --password '${POSTGRES_PASSWORD}' \
  --database analytics
```

```yaml [YAML]
pipelines:
  bikehire:
    data_source: data_sources.sample
    data_sink: data_sinks.warehouse

data_sinks:
  warehouse:
    Postgres:
      host: localhost
      port: 5432
      user: skippr
      password: ${POSTGRES_PASSWORD}
      database: analytics
```

:::

### Add a SkipprLake destination and its schema sink

Nested `object_store` fields are flattened into `--object-store-*` flags. The schema sink is added second and copies the data sink's configuration.

```bash
export OBJECTS_S3_ENDPOINT='https://<account>.r2.cloudflarestorage.com'
export OBJECTS_ACCESS_KEY_ID='replace-me'
export OBJECTS_SECRET_ACCESS_KEY='replace-me'
```

```bash
skipprd connect data-sink skippr-lake \
  --pipeline bikehire \
  --name lake \
  --catalog-table my-iceberg-catalog \
  --warehouse 's3://my-iceberg-warehouse/' \
  --table-namespace bronze \
  --object-store-type r2 \
  --object-store-endpoint '${OBJECTS_S3_ENDPOINT}' \
  --object-store-access-key-id '${OBJECTS_ACCESS_KEY_ID}' \
  --object-store-secret-access-key '${OBJECTS_SECRET_ACCESS_KEY}'

skipprd connect schema-sink skippr-lake --pipeline bikehire --name lake_schema
```

The resulting `skippr.yml` contains:

```yaml
data_sinks:
  lake:
    schema_sink: schema_sinks.lake_schema
    SkipprLake:
      catalog_table: my-iceberg-catalog
      warehouse: s3://my-iceberg-warehouse/
      table_namespace: bronze
      object_store:
        type: r2
        endpoint: ${OBJECTS_S3_ENDPOINT}
        access_key_id: ${OBJECTS_ACCESS_KEY_ID}
        secret_access_key: ${OBJECTS_SECRET_ACCESS_KEY}

schema_sinks:
  lake_schema:
    SkipprLake:
      catalog_table: my-iceberg-catalog
      warehouse: s3://my-iceberg-warehouse/
      table_namespace: bronze
      object_store:
        type: r2
        endpoint: ${OBJECTS_S3_ENDPOINT}
        access_key_id: ${OBJECTS_ACCESS_KEY_ID}
        secret_access_key: ${OBJECTS_SECRET_ACCESS_KEY}
```

### Add an Athena destination with a Glue schema sink

Add the data sink first, then the schema sink.

```bash
skipprd connect data-sink athena \
  --pipeline bikehire \
  --name athena \
  --s3-bucket your-output-bucket \
  --s3-prefix data/bikehire \
  --athena-workgroup-name primary \
  --athena-results-s3-bucket your-athena-results

skipprd connect schema-sink glue \
  --pipeline bikehire \
  --name glue \
  --s3-bucket your-output-bucket \
  --s3-prefix data/bikehire \
  --glue-database-name skippr_quickstart \
  --athena-workgroup-name primary \
  --athena-results-s3-bucket your-athena-results
```

### Pass a map value

```bash
skipprd connect data-source http-client \
  --pipeline orders \
  --name orders_api \
  --url https://api.example.com/orders \
  --headers '{Accept: application/json, X-Team: data}'
```

Writes:

```yaml
data_sources:
  orders_api:
    HttpClient:
      url: https://api.example.com/orders
      headers:
        Accept: application/json
        X-Team: data
```

### Check the result

```bash
skipprd doctor
```

### Deadletter sinks

`connect` does not write `deadletter_sinks`. Add a deadletter sink in Python with `Config.deadletter_sink(...)` or directly in `skippr.yml`. See [Deadletters](/concepts/deadletters).

## Troubleshooting

| Message | Cause | Fix |
|---|---|---|
| `secret field 'password' must be an unresolved ${ENV} reference like ${NAME}` | You passed a plaintext secret, or your shell expanded `${NAME}` before Skipprd saw it. | Pass `'${NAME}'` in single quotes and `export NAME=...` separately. |
| `plugin name 'warehouse' already uses Postgres, not Snowflake` | That `--name` is already a different connector in `skippr.yml`. | Use a new `--name`, or remove the old entry. |
| `connect schema-sink requires a data_sink on this pipeline` | The schema sink was added before the destination. | Run `connect data-sink` for the pipeline first. |
| `paired ... configs must be equal before a write changes them` | A paired data sink and schema sink were edited apart by hand. | Make the two blocks in `skippr.yml` identical, then rerun. |
| `--store-name requires --store-type` | `--store-name` was passed alone. | Add `--store-type dynamodb` or `--store-type cloud-tables`. |
| `entry name "a.b" must be non-empty and contain no '.'` | Entry names are referenced as `<section>.<name>`, so dots are not allowed. | Use underscores or hyphens. |
| `connect requires --pipeline` | No `--pipeline` and no terminal to prompt on (for example in CI). | Pass `--pipeline` and `--name` explicitly. |
| `... is not a YAML (.yml / .yaml) file` | `--config` points at a JSON or TOML file. | `connect` writes YAML only. Point `--config` at a `.yml` file. |

## Next steps

- [skippr.yml reference](/configuration/skippr-yml)
- [skipprd doctor](/cli/doctor)
- [skipprd discover](/cli/discover)
- [Python](/python)
