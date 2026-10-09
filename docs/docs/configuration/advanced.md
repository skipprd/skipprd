---
title: Advanced settings
description: Set the data directory, object-storage bucket, schema approval, and sync interval for production.
---

# Advanced settings

Most pipelines only need `skippr.yml` and a few secrets. Use these engine settings when you move off a laptop: a persistent data directory, an S3 bucket for schema and metadata, or a long-running sync loop.

Connector options stay on the connector pages. This page is process-wide.

## Data directory

`DATA_DIR` (default `./data`) holds the local write-ahead log when `WAL_STORAGE=disk`, plus local resume state. Skipprd creates it if it is missing.

```bash
export DATA_DIR=/var/lib/skipprd
```

You can also set `data_dir` on a pipeline in `skippr.yml`. Keep the directory on a persistent disk. See [WAL and buffering](/configuration/buffering).

## Object storage for schema and metadata

`SKIPPR_S3_BUCKET` is the bucket Skipprd uses for pipeline schema, uploaded run config, and (when `WAL_STORAGE=s3`) WAL segments unless you set `SKIPPR_WAL_S3_BUCKET`.

```bash
export SKIPPR_S3_BUCKET=acme-skipprd-state
```

Required for production when schema is not stored only on the local disk.

`SKIPPRD_EL_STORAGE_MODE` chooses where extract/load metadata and namespace stats persist:

| Value | Where metadata lives | Use when |
|---|---|---|
| `s3` (default) | `SKIPPR_S3_BUCKET` | Shared or remote state |
| `local` | under `DATA_DIR` | A developer machine with no bucket, for example local dbt |

YAML equivalent: `skippr.skipprd_el_storage_mode`.

## Environment label

`SKIPPR_ENV` (default `prod`) is a label on logs and metadata. It does not change behaviour.

## Schema changes

`SCHEMA_AUTO_APPROVE` (default `true`) applies discovered schema changes during discover and sync. Set it to `false` when you want to review a change with `skipprd schema` before it lands. See [Schema discovery and evolution](/concepts/schema).

## How often sync repeats

A bare `skipprd sync` already loops every `sync_frequency_seconds` on the pipeline (default 900). `SYNC_FREQUENCY`, when set, is a process-wide interval in seconds for that loop.

Prefer the pipeline field in `skippr.yml` so each pipeline can differ.

## Chaos testing

`SKIPPR_CHAOS_MODE=yes` kills the process with SIGKILL at a random point 15–60 seconds into a run. Use it only to prove [exactly-once delivery](/concepts/exactly-once) under failure. Leave it unset in production.

## Next steps

- [WAL and buffering](/configuration/buffering)
- [State store](/configuration/skippr-store)
- [Logging](/operations/logging)
- [skippr.yml](/configuration/skippr-yml)
