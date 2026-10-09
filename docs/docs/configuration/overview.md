---
title: Configuration overview
description: Understand how skippr.yml, environment variables, and CLI flags combine, which one wins, and where each setting is documented.
---

# Configuration overview

Skipprd takes configuration from three places. Each has one job:

| Layer | Use it for | Example |
|---|---|---|
| `skippr.yml` | What you move: sources, destinations, pipelines, transforms, and engine settings you want under version control. | `data_sources`, `pipelines.orders.transform` |
| Environment variables | Secrets, referenced from `skippr.yml` as `${NAME}`, and settings that describe the machine Skipprd runs on. | `APP_DB_PASSWORD`, `WAL_STORAGE`, `DATA_DIR` |
| CLI flags | What to do on this run, plus a few per-run overrides. | `--pipeline orders`, `--once`, `--log`, `--wal-storage s3` |

The usual split: commit `skippr.yml`, keep secrets in your environment or a gitignored `.env.local`, and set operator settings in your scheduler, container, or service definition.

Connectors are always configured in `skippr.yml`, through Python `Config`, or with `skipprd connect`. Each connector page lists its fields.

## Which value wins

When the same setting can come from more than one place, Skipprd resolves it like this:

| Setting | Order, first wins |
|---|---|
| Pipeline settings such as `buffer_threshold_bytes`, `data_dir`, `sync_frequency_seconds`, and `transform.*` | `skippr.yml` pipeline key → environment variable → built-in default |
| `skippr.workspace`, `skippr.tenant`, `skippr.skippr_s3_bucket`, `skippr.skipprd_el_storage_mode` | `skippr.yml` → environment variable → built-in default |
| `skippr.wal_s3_bucket`, `skippr.store.type`, `skippr.store.name` | `skippr.yml` → CLI flag → environment variable → built-in default |
| `WAL_STORAGE` | `--wal-storage` flag → `WAL_STORAGE` environment variable → `disk` |
| Which config file to read | `--config` → `SKIPPR_CONFIG_FILE` → `./skippr.yml` and the other default names |
| Which pipeline to run | `--pipeline` → `PIPELINE_NAME` |

A value in `skippr.yml` always beats the environment. To vary a setting per machine, leave it out of `skippr.yml` and set the environment variable instead.

## Global flags

These flags work with any command:

| Flag | Sets | Notes |
|---|---|---|
| `--config <path>` | The config file to read | Same as `SKIPPR_CONFIG_FILE`. |
| `--log [level]` | Log output to stderr | `trace`, `debug`, `info` (default), `warn`, or `error`. See [Logging](/operations/logging). |
| `--wal-storage <disk\|s3\|clustered>` | WAL backend | Overrides `WAL_STORAGE`. See [WAL and buffering](/configuration/buffering). |
| `--wal-s3-bucket <bucket>` | WAL bucket for `s3` | Overrides `SKIPPR_WAL_S3_BUCKET`. |
| `--store-type <sled\|dynamodb\|cloud-tables>` | State store | Overrides `SKIPPR_STORE_TYPE`. See [State store](/configuration/skippr-store). |
| `--store-name <table>` | State store table | Overrides `SKIPPR_STORE_NAME`. |

`--workspace`, `--tenant`, `--storage-mode`, and `--skippr-s3-bucket` take effect with `skipprd connect`, which writes them into the `skippr:` section of `skippr.yml`. Set them in the file for every other command.

## Example: one file, two machines

Keep the pipeline in `skippr.yml` and let each machine supply its own settings.

On a laptop, use the defaults: the WAL and state stay in `./data`.

```bash
export APP_DB_PASSWORD='your-postgres-password'
skipprd sync --pipeline orders --once
```

In a container with no persistent disk, keep the WAL in S3 and progress in DynamoDB:

```bash
export APP_DB_PASSWORD='your-postgres-password'
export WAL_STORAGE=s3
export SKIPPR_WAL_S3_BUCKET=acme-skipprd-wal
export SKIPPR_STORE_TYPE=dynamodb
export SKIPPR_STORE_NAME=skipprd-state
skipprd sync --pipeline orders --once
```

This works only if `skippr.yml` does not set `skippr.store` or `skippr.wal_s3_bucket`, because file values win.

## Where to go

| To | Read |
|---|---|
| Look up any key in the file | [skippr.yml reference](/configuration/skippr-yml) |
| Define a pipeline, its schedule, and its buffers | [Pipelines](/configuration/pipeline) |
| Pick and wire a source | [Sources](/configuration/input) |
| Pick and wire a destination, schema sink, or deadletter sink | [Destinations](/configuration/output) |
| Split, partition, flatten, or sort records | [Transforms](/configuration/transforms) |
| Choose disk, S3, or clustered WAL | [WAL and buffering](/configuration/buffering) |
| Keep sync progress in DynamoDB | [State store](/configuration/skippr-store) |
| Look up every other engine setting | [Advanced settings](/configuration/advanced) |

## Next steps

- [skippr.yml reference](/configuration/skippr-yml)
- [How Skipprd works](/concepts/how-it-works)
- [CLI overview](/cli/overview)
