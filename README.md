# Skipprd

[![CI](https://github.com/skipprd/skipprd/actions/workflows/ci.yml/badge.svg)](https://github.com/skipprd/skipprd/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/skipprd/skipprd)](https://github.com/skipprd/skipprd/releases)

Skipprd is a self-hosted ELT engine. Describe a source and a destination in `skippr.yml`, then run `skipprd discover` and `skipprd sync`. Skipprd infers the schema, keeps up as it changes, and buffers every batch in a write-ahead log so a crash never loses or duplicates committed data.

**Full documentation: [skippr.io](https://skippr.io)**

This repository is **source-available** under [PolyForm Shield 1.0.0](./LICENSE), not OSI open source.

Licensor Line of Business: Skipprd ELT engine (https://skippr.io)

## Install

Published builds cover macOS arm64 and Linux x86_64.

```bash
# CLI: Homebrew
brew tap skipprd/tap
brew install skipprd

# CLI: install script
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | sh

# Python package (the same engine, as `import skippr`)
pip install skippr
```

Archives are also on [GitHub Releases](https://github.com/skipprd/skipprd/releases). See [Install](https://skippr.io/getting-started/install) for options and troubleshooting.

## Quick start

Describe a pipeline in `skippr.yml` with Python, the CLI, or by hand. This one reads sample JSON from S3 and lands it in PostgreSQL:

```yaml
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
      user: postgres
      password: ${POSTGRES_PASSWORD}
      database: analytics
```

Secrets are `${NAME}` references, read from the environment (or a `.env` file next to `skippr.yml`) when the pipeline starts. Then run:

```bash
export POSTGRES_PASSWORD="your-password"
skipprd discover --pipeline bikehire
skipprd sync --pipeline bikehire --once
```

The same pipeline from Python:

```python
import skippr
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
        user="postgres",
        password=EnvRef("POSTGRES_PASSWORD"),
        database="analytics",
    ),
)
bikehire = cfg.pipeline("bikehire", Pipeline(data_source=sample, data_sink=warehouse))
cfg.save("skippr.yml")

s = skippr.Session(bikehire)
s.discover()
s.sync(once=True)
```

Or add each connector from the command line with [`skipprd connect`](https://skippr.io/cli/connect):

```bash
skipprd --workspace quickstart --storage-mode local connect data-source s3 \
  --pipeline bikehire --name sample \
  --s3-bucket skippr-public-sample-data --s3-prefix bike-hire

skipprd connect data-sink postgres \
  --pipeline bikehire --name warehouse \
  --host localhost --user postgres \
  --password '${POSTGRES_PASSWORD}' --database analytics
```

Step-by-step guides: [S3 to Athena](https://skippr.io/getting-started/quickstart), [Snowflake](https://skippr.io/getting-started/quickstart-snowflake), [PostgreSQL](https://skippr.io/getting-started/quickstart-postgres), [BigQuery](https://skippr.io/getting-started/quickstart-bigquery).

## Learn more

| Topic | Docs |
|---|---|
| Every `skippr.yml` setting | [skippr.yml reference](https://skippr.io/configuration/skippr-yml) |
| Sources and destinations | [Connector catalog](https://skippr.io/connectors/) |
| Commands (`discover`, `sync`, `query`, `doctor`, …) | [CLI reference](https://skippr.io/cli/overview) |
| Python `Config` and `Session` | [Python](https://skippr.io/python) |
| WAL, buffering, and state | [Run in production](https://skippr.io/configuration/buffering) |
| SQL extensions for `skipprd query` | [sql-docs.md](sql-docs.md) |

## Build from source

You need the Rust toolchain pinned in `rust-toolchain.toml`, `protoc` (the protobuf compiler), and OpenSSL development headers.

```bash
cargo build
cargo test --workspace --lib --exclude skipprd-python
./scripts/test-python.sh
cargo fmt --all -- --check
cargo clippy
```

Contributor and agent guidance is in [AGENTS.md](AGENTS.md). Performance notes are in [PERFORMANCE.md](PERFORMANCE.md).

## Related products

Cloud `skippr-cloud` is a different binary ([install.skippr.io](https://install.skippr.io)). Data Engineer is [`sde`](https://data-engineer.skippr.io). Managed Cloud ELT is [skippr.io/elt](https://skippr.io/elt/).
