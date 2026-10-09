---
title: "Quickstart: S3 to Athena"
description: Load sample JSON from S3 into a Parquet table you can query in Amazon Athena, using skippr.yml, skipprd discover, and skipprd sync.
---

# Quickstart: S3 to Athena

In about ten minutes you will load public sample JSON from S3 into your own S3 bucket as Parquet, registered as a table in the AWS Glue Data Catalog, and query it in Amazon Athena.

Skipprd reads the JSON, infers its schema, creates the Glue database and table for you, and writes the data. You only provide a bucket and permissions.

## Before you begin

You need:

- **Skipprd installed.** Check with `skipprd --version`, or `python -c "import skippr"` if you use Python. See [Install](/getting-started/install).
- **An AWS account and credentials** for an IAM principal that can:
  - read `s3://skippr-public-sample-data/bike-hire/` (`s3:ListBucket`, `s3:GetObject`);
  - read and write your output bucket (`s3:ListBucket`, `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject`);
  - create and update Glue databases, tables, and partitions;
  - run Athena queries in your workgroup.
- **An S3 bucket for the output** in the region you will query from. This guide uses one bucket, `your-output-bucket`, for both the Parquet data and Athena query results.

Export your credentials, then create an empty working directory. Skipprd reads `skippr.yml` from the current directory, so run every command from there.

```bash
export AWS_ACCESS_KEY_ID="your-access-key-id"
export AWS_SECRET_ACCESS_KEY="your-secret-access-key"
export AWS_DEFAULT_REGION="us-east-1"

mkdir skippr-athena && cd skippr-athena
```

Any standard AWS credential source works instead of access keys, such as `AWS_PROFILE` or an instance role. Use the region your output bucket and Athena workgroup live in.

## 1. Describe the pipeline

A pipeline has a **source** (where data comes from), a **destination** (where it lands), and for Athena a **schema sink** (where table definitions go — here, the Glue Data Catalog). Each tab below writes the same `skippr.yml`. Replace `your-output-bucket` with your bucket name.

::: code-group

```python [Python]
from skippr import Config, DataSinkAthena, DataSourceS3, LocalStorage, Pipeline, SchemaSinkGlue

cfg = Config().workspace("quickstart").storage(LocalStorage())
sample = cfg.data_source(
    "sample",
    DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire"),
)
glue = cfg.schema_sink(
    "glue",
    SchemaSinkGlue(
        s3_bucket="your-output-bucket",
        s3_prefix="data/bikehire",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="your-output-bucket",
        glue_database_name="skippr_quickstart",
    ),
)
athena = cfg.data_sink(
    "athena",
    DataSinkAthena(
        s3_bucket="your-output-bucket",
        s3_prefix="data/bikehire",
        athena_workgroup_name="primary",
        athena_results_s3_bucket="your-output-bucket",
    ),
    schema_sink=glue,
)
cfg.pipeline("bikehire", Pipeline(data_source=sample, data_sink=athena))
cfg.save("skippr.yml")
```

```bash [CLI]
skipprd --workspace quickstart --storage-mode local connect data-source s3 \
  --pipeline bikehire \
  --name sample \
  --s3-bucket skippr-public-sample-data \
  --s3-prefix bike-hire

skipprd connect data-sink athena \
  --pipeline bikehire \
  --name athena \
  --s3-bucket your-output-bucket \
  --s3-prefix data/bikehire \
  --athena-workgroup-name primary \
  --athena-results-s3-bucket your-output-bucket

skipprd connect schema-sink glue \
  --pipeline bikehire \
  --name glue \
  --s3-bucket your-output-bucket \
  --s3-prefix data/bikehire \
  --athena-workgroup-name primary \
  --athena-results-s3-bucket your-output-bucket \
  --glue-database-name skippr_quickstart
```

```yaml [YAML]
skippr:
  workspace: quickstart
  skipprd_el_storage_mode: local

pipelines:
  bikehire:
    data_source: data_sources.sample
    data_sink: data_sinks.athena

data_sources:
  sample:
    S3:
      s3_bucket: skippr-public-sample-data
      s3_prefix: bike-hire

data_sinks:
  athena:
    schema_sink: schema_sinks.glue
    Athena:
      s3_bucket: your-output-bucket
      s3_prefix: data/bikehire
      athena_workgroup_name: primary
      athena_results_s3_bucket: your-output-bucket

schema_sinks:
  glue:
    Glue:
      s3_bucket: your-output-bucket
      s3_prefix: data/bikehire
      athena_workgroup_name: primary
      athena_results_s3_bucket: your-output-bucket
      glue_database_name: skippr_quickstart
```

:::

What the settings mean:

- `athena_results_s3_bucket` is a bucket **name**, not an `s3://` URI.
- `glue_database_name` on the Glue schema sink names the database the table is registered in. The Athena destination takes it from the linked schema sink, so you set it once.
- With the CLI, add the destination before the schema sink: `connect schema-sink` attaches to the pipeline's existing destination.
- `LocalStorage()` (`skipprd_el_storage_mode: local`) keeps Skipprd's own state, such as the discovered schema and how far it has read, in `./data`. That is fine for a trial; for production, see [State store](/configuration/skippr-store).

## 2. Discover the schema

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

`discover` samples the JSON objects under the prefix and records each field's name and type. It does not write to your bucket. `skipprd schema` prints the fields it found, which become the columns of your Athena table.

The first command that uses a connector downloads it, so the first run takes a little longer.

## 3. Sync

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

Sync reads every object under the prefix and writes it to Skipprd's write-ahead log (WAL) first. A crash retries that committed batch. Athena overwrites the same files, so you do not get a second copy. Skipprd then creates the `skippr_quickstart` Glue database and the `bikehire` table if they do not exist, and writes Parquet files under `s3://your-output-bucket/data/bikehire/bikehire/`.

`--once` (`once=True`) runs a single pass and exits. Without it, sync keeps running and checks the source for new objects on a schedule (every 900 seconds by default; set `sync_frequency_seconds` on the pipeline to change it).

## Check it worked

Open the [Athena query editor](https://console.aws.amazon.com/athena/) in the same region, select the `primary` workgroup and the `skippr_quickstart` database, and run:

```sql
SELECT count(*) AS row_count FROM skippr_quickstart.bikehire;
```

You should see a non-zero `row_count`. Then look at the data:

```sql
SELECT * FROM skippr_quickstart.bikehire LIMIT 10;
```

The columns match the fields `skipprd schema` printed in step 2. The table is named after the pipeline, `bikehire`.

Run `skipprd sync --pipeline bikehire --once` again and repeat the count. It does not change: Skipprd remembers which objects it has already read.

## Troubleshooting

- **`AccessDenied` reading `skippr-public-sample-data`** — your credentials are missing or lack S3 read access. Run `aws sts get-caller-identity` to confirm which principal Skipprd uses, then grant `s3:ListBucket` and `s3:GetObject` on the sample bucket.
- **`AccessDenied` on a Glue `CreateDatabase`, `CreateTable`, or partition call** — the principal cannot manage the Glue Data Catalog. Grant Glue create, get, and update permissions on the `skippr_quickstart` database and its tables.
- **`AccessDenied` writing to `your-output-bucket`** — grant `s3:PutObject` (and `s3:ListBucket`, `s3:GetObject`, `s3:DeleteObject`) on the bucket.
- **Athena cannot find `skippr_quickstart`** — the console is in a different region from the one you synced to. Switch to the region in `AWS_DEFAULT_REGION`.
- **Athena asks for a query result location** — the `primary` workgroup has no output location. In the workgroup or query editor settings, set it to `s3://your-output-bucket/athena-results/`.
- **The count is zero or the table is missing after sync** — sync did not finish. Rerun with `--log` (or `--log debug`) to see each step and the error that stopped it. Records that fail to convert go to a [deadletter](/concepts/deadletters) instead of the table.
- **The first run hangs while fetching a connector** — the machine cannot reach `install.skippr.io` over HTTPS. See [Install](/getting-started/install).

For more, see [Troubleshooting](/operations/troubleshooting).

## Next steps

- [How Skipprd works](/concepts/how-it-works) — the path from source to WAL to destination.
- [Athena destination](/connectors/outputs/athena) and [Glue schema sink](/connectors/schema_sinks/glue) — every option.
- [S3 source](/connectors/inputs/s3) — point the pipeline at your own bucket and prefix.
- [Transforms](/configuration/transforms) — split records into tables, partition by time, and order rows.
- [Exactly-once delivery](/concepts/exactly-once) — what a crash or retry does at each destination.
