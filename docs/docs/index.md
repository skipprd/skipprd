---
title: Skipprd
description: Skipprd is a self-hosted ELT engine. Install it, describe a source and destination in skippr.yml, and land your first data in minutes.
---

# Skipprd

Skipprd moves data from your sources — databases, files, streams, and SaaS APIs — into your warehouse or lake. It runs on your own machines, as the `skipprd` command or as the `skippr` Python package. Both run the same engine.

You describe a pipeline once in `skippr.yml`: where data comes from, and where it should land. Skipprd does the rest:

- **Discovers the schema.** It samples the source and infers field names and types, including nested objects and arrays, the same way every time.
- **Keeps up when data changes.** A new field, a field that changes type, or a nested object that grows a new array lands without breaking the tables you already query.
- **Does not lose committed data.** Skipprd writes each batch to its write-ahead log (WAL) before the destination. A crash retries from the WAL. Whether a retry can duplicate a row depends on the destination — see [Exactly-once delivery](/concepts/exactly-once).
- **Replicates database changes.** [Change data capture](/cdc/) keeps a destination table at the source's current state, including deletes.

Connectors download on first use from `install.skippr.io`, so the engine stays small and each connector updates on its own.

## Who it is for

- **Developers and analysts** who want source data in a warehouse they can query, without writing extraction code.
- **Operators** who install, schedule, monitor, and recover pipelines on their own infrastructure.

## Install

Skipprd runs on **macOS arm64** and **Linux x86_64**. Use Homebrew or the install script for the `skipprd` command, or `pip` for the Python package.

::: code-group

```bash [Homebrew]
brew tap skipprd/tap
brew install skipprd
skipprd --version
```

```bash [install.sh]
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | sh
skipprd --version
```

```bash [pip]
pip install skippr
python -c "import skippr; print(skippr.Session)"
```

:::

The `pip` package gives you the Python API (`import skippr`). It does not add the `skipprd` command. See [Install](/getting-started/install) for user-local installs, pinned versions, and fixes for common problems.

## Your first pipeline in 5 minutes

Read public sample JSON from S3, discover its schema, sync it, and count the rows with SQL. You do not need a warehouse: without a destination, Skipprd keeps the data in its local WAL and you can query it there.

You need AWS credentials that can read S3. Any AWS account works.

```bash
mkdir skippr-first-pipeline && cd skippr-first-pipeline
export AWS_ACCESS_KEY_ID="your-access-key-id"
export AWS_SECRET_ACCESS_KEY="your-secret-access-key"
export AWS_DEFAULT_REGION="us-east-1"
```

**1. Describe the pipeline.** Each tab writes the same `skippr.yml` in the current directory.

::: code-group

```python [Python]
from skippr import Config, DataSourceS3, LocalStorage, Pipeline

cfg = Config().workspace("quickstart").storage(LocalStorage())
sample = cfg.data_source(
    "sample",
    DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire"),
)
cfg.pipeline("bikehire", Pipeline(data_source=sample))
cfg.save("skippr.yml")
```

```bash [CLI]
skipprd --workspace quickstart --storage-mode local connect data-source s3 \
  --pipeline bikehire \
  --name sample \
  --s3-bucket skippr-public-sample-data \
  --s3-prefix bike-hire
```

```yaml [YAML]
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

:::

`LocalStorage()` (`skipprd_el_storage_mode: local`) keeps Skipprd's state in `./data`, so you do not need a state bucket to try it out.

**2. Discover, sync, and query.**

::: code-group

```python [Python]
import skippr

s = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
s.discover()
s.sync(once=True)
print(s.query("SELECT count(*) FROM bikehire"))
```

```bash [CLI]
skipprd discover --pipeline bikehire
skipprd schema --pipeline bikehire
skipprd sync --pipeline bikehire --once
skipprd query --plain --sql "SELECT count(*) FROM bikehire"
```

:::

`discover` samples the JSON and records every field and its type. `schema` prints what it found. `sync --once` reads the source in a single pass and exits; without `--once`, sync keeps running and picks up new objects. The query returns the number of rows Skipprd ingested.

## Where to go next

- **Land data in a warehouse:** [S3 to Athena](/getting-started/quickstart), [Snowflake](/getting-started/quickstart-snowflake), [PostgreSQL](/getting-started/quickstart-postgres), or [BigQuery](/getting-started/quickstart-bigquery).
- **Use Skipprd from Python:** [Python](/python).
- **Understand what happens to your data:** [How Skipprd works](/concepts/how-it-works) and [Schema discovery and evolution](/concepts/schema).
- **Connect your own systems:** browse [data sources](/connectors/) and the [`skippr.yml` reference](/configuration/skippr-yml).
- **Run it in production:** [WAL and buffering](/configuration/buffering), [State store](/configuration/skippr-store), [Logging](/operations/logging), and [Troubleshooting](/operations/troubleshooting).
- **Look up a command:** [CLI reference](/cli/overview).
