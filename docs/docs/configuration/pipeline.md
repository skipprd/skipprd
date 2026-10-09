---
title: Pipelines
description: Define a pipeline in skippr.yml, wire its source and destinations, set its buffers and schedule, and run it by name.
---

# Pipelines

A pipeline reads from one source and writes to up to two destinations: a data sink for good records and a deadletter sink for records that fail. You define pipelines under `pipelines:` in `skippr.yml` and run them by name.

## Define a pipeline

This pipeline reads JSON files, partitions them by day, and loads them into Postgres. Failed records go to local files.

::: code-group

```python [Python]
from skippr import (
    Config,
    DataSinkFile,
    DataSinkPostgres,
    DataSourceFile,
    EnvRef,
    LocalStorage,
    Pipeline,
    Transform,
)

cfg = Config().workspace("dev").storage(LocalStorage())
events = cfg.data_source("events", DataSourceFile(path="./events"))
landing = cfg.data_sink(
    "landing",
    DataSinkPostgres(
        host="localhost",
        user="skippr",
        password=EnvRef("POSTGRES_PASSWORD"),
        database="analytics",
        schema="raw",
    ),
)
rejects = cfg.deadletter_sink("rejects", DataSinkFile(output_dir="./deadletters"))

cfg.pipeline(
    "events",
    Pipeline(
        data_source=events,
        data_sink=landing,
        deadletter_sink=rejects,
        transform=Transform(batch_time_fields="created_at", batch_time_unit="day"),
    ),
)
cfg.save("skippr.yml")
```

```yaml [YAML]
skippr:
  workspace: dev
  skipprd_el_storage_mode: local

pipelines:
  events:
    data_source: data_sources.events
    data_sink: data_sinks.landing
    deadletter_sink: deadletter_sinks.rejects
    transform:
      batch_time_fields: created_at
      batch_time_unit: day

data_sources:
  events:
    File:
      path: ./events

data_sinks:
  landing:
    Postgres:
      host: localhost
      user: skippr
      password: ${POSTGRES_PASSWORD}
      database: analytics
      schema: raw

deadletter_sinks:
  rejects:
    File:
      output_dir: ./deadletters
```

:::

`skipprd connect` creates the pipeline entry and wires its source and data sink for you (see [Sources](/configuration/input)). Set the other pipeline keys, such as `transform` and `deadletter_sink`, in YAML or Python.

The pipeline name, `events` here, is the YAML key. Use it with `--pipeline` on the CLI and with `Config.get_pipeline("events")` in Python. The entry names `events`, `landing`, and `rejects` are logical names you choose; see the [skippr.yml reference](/configuration/skippr-yml).

## Run it

::: code-group

```python [Python]
import skippr

session = skippr.Session(skippr.Config.discover().get_pipeline("events"))
session.discover()
session.sync(once=True)
```

```bash [CLI]
export POSTGRES_PASSWORD='your-postgres-password'
skipprd discover --pipeline events
skipprd sync --pipeline events --once
```

:::

`--once` runs a single pass and exits, which suits cron and other schedulers. Without it, `skipprd sync` keeps running until you stop it.

## Pipeline keys

| Key | Default | Description |
|---|---|---|
| `data_source` | Required | The source to read, as `data_sources.<name>`. |
| `data_sink` | none | Where good records go, as `data_sinks.<name>`. Without it, records stay in the write-ahead log (WAL), where you can still query them with `skipprd query`. |
| `deadletter_sink` | none | Where records that fail to transform or load go, as `deadletter_sinks.<name>`. Without it, those records are discarded and counted in the log. See [Deadletters](/concepts/deadletters). |
| `transform` | none | Split records into tables, partition, flatten, and sort. See [Transforms](/configuration/transforms). |
| `cdc` | none | Turns on change data capture for this pipeline. See [Change data capture](/cdc/). |
| `buffer_threshold_bytes` | `10485760` (10 MiB) | Flush buffered records to the WAL at this size. Env fallback `BUFFER_THRESHOLD_BYTES`. See [WAL and buffering](/configuration/buffering). |
| `buffer_threshold_seconds` | `60` | Flush buffered records to the WAL after this many seconds. Env fallback `BUFFER_THRESHOLD_SECONDS`. |
| `sync_frequency_seconds` | `900` | Minimum gap between runs of this pipeline when `skipprd sync` runs every pipeline in a loop. Env fallback `SYNC_FREQUENCY`. |
| `data_dir` | `./data` | Local directory for this pipeline's WAL and local state. Env fallback `DATA_DIR`. |
| `chaos_mode` | `false` | Kills Skipprd partway through a run to test recovery. Never enable in production. See [Advanced settings](/configuration/advanced). |

A value set here beats the environment variable, which beats the default.

## Workspaces and pipeline identity

Skipprd saves each pipeline's progress under its workspace and pipeline name (`skippr.workspace` and the `pipelines:` key). That has two consequences:

- **Renaming starts over.** Change either name and Skipprd treats it as a new pipeline: it reads the source from the beginning and writes the data again. Point the destination at a new schema or prefix first if you do not want duplicates.
- **One writer at a time.** Two `skipprd sync` processes for the same workspace and pipeline cannot write at once. The second waits about 30 seconds, then stops with `pipeline writer lease unavailable`.

Reserved pipeline names: `deadletters`, `wal`, `_skippr`, `skippr`, `metadata`.

## Run every pipeline

Leave out `--pipeline` and `skipprd sync` runs each pipeline in the file in turn:

```bash
skipprd sync --once
```

With `--once`, it runs each pipeline once and exits. Without `--once`, it keeps cycling and skips any pipeline that ran less than `sync_frequency_seconds` ago, logging `Pipeline '<name>' last ran <n> seconds ago, skipping for <m> seconds.`

`skipprd discover` always needs a pipeline name, from `--pipeline` or `PIPELINE_NAME`.

## Next steps

- [Sources](/configuration/input) — choose and wire a source.
- [Destinations](/configuration/output) — choose a destination, schema sink, and deadletter sink.
- [Transforms](/configuration/transforms) — shape records before they land.
- [skipprd sync](/cli/sync) — every sync flag.
