---
title: skipprd sync
description: Ingest data from a pipeline's source, buffer it durably in the WAL, and land it in your destination, once or continuously.
---

# skipprd sync

`skipprd sync` moves data. It reads new records from the pipeline's source, buffers each batch durably in its write-ahead log (WAL), and writes the batches to your destination. Offsets are committed with the data, so a crash or restart picks up where it left off without losing or duplicating committed records. Run it with `--once` from a scheduler (cron, Airflow, CI) to process what is available and exit, or without `--once` as a long-running service. A pipeline without a `data_sink` keeps its data in the WAL, where `skipprd query` and `skipprd df` can read it.

## Usage

::: code-group

```python [Python]
import skippr

session = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
session.sync(once=True)
```

```bash [CLI]
skipprd sync [--pipeline <name>] [--once] [--output progress|text|json] [--log [LEVEL]] [--config <path>]
```

:::

## Options

| Flag | Default | Description |
|---|---|---|
| `-p`, `--pipeline <NAME>` | `PIPELINE_NAME` env var | Pipeline to sync. If neither is set, Skipprd syncs every pipeline in `skippr.yml`; see [Syncing every pipeline](#syncing-every-pipeline). |
| `--once` | Off | Read what the source has now, land it, and exit. Without it, sync keeps running until you stop it. |
| `--output <MODE>` | `progress` | `progress` shows a live progress display on a terminal. `text` prints one plain line per event. `json` prints one JSON object per line. |
| `--config <PATH>` | `./skippr.yml` | Config file to read. |
| `--log [LEVEL]` | Off | Print logs to stderr. `--log` alone means `info`. Turning on logs hides the progress display. |
| `--wal-storage <MODE>` | `disk` | WAL backend: `disk`, `s3`, or `clustered`. `clustered` does not support `--once`. See [WAL and buffering](/configuration/buffering). |
| `--wal-s3-bucket <BUCKET>` | State bucket | WAL bucket for `--wal-storage s3`, when `skippr.yml` does not set `skippr.wal_s3_bucket`. |
| `--store-type <TYPE>` | `sled` | State store for offsets and leases, when `skippr.yml` does not set `skippr.store.type`. See [State store](/configuration/skippr-store). |
| `--store-name <NAME>` | None | Table name for the `dynamodb` or `cloud-tables` state store. |

See [CLI overview](/cli/overview) for the other global flags.

### Pipeline settings that affect sync

These live on the pipeline in `skippr.yml` (see [Pipelines](/configuration/pipeline)):

| Key | Default | Effect |
|---|---|---|
| `sync_frequency_seconds` | `900` | Minimum time between runs of a pipeline when you sync every pipeline at once. |
| `data_dir` | `DATA_DIR`, else `./data` | Where Skipprd keeps the pipeline's local working data. |

## What sync does

1. Loads the pipeline's saved schema. If there is none yet, it runs [discover](/cli/discover) first.
2. Skips the pipeline if it was disabled with `DISABLE PIPELINE`.
3. Takes the pipeline's writer lease, so only one `sync` writes a pipeline at a time.
4. Reads the source in batches and buffers each batch in the WAL.
5. Writes buffered batches to the destination, if the pipeline has one, and publishes schema changes to the schema sink.
6. With `--once`, drains everything still buffered, then exits.

## Examples

### Run once from a scheduler

```bash
skipprd sync --pipeline bikehire --once --output text
```

```text
Sync started: pipeline=bikehire
[start] Ingesting
[done]  Ingesting
[start] Finalising
[done]  Finalising
Sync complete: pipeline=bikehire namespaces=1 rows=48213 elapsed=21874ms
```

`rows` counts rows written to the destination during this run.

A cron entry that syncs every 15 minutes, keeping JSON events and errors in separate files:

```bash
*/15 * * * * cd /opt/skipprd && skipprd sync --pipeline bikehire --once --output json >> sync-events.jsonl 2>> sync-errors.log
```

Check the exit code in your scheduler: `0` means the run completed, `1` means it failed.

### Run as a long-lived service

```bash
skipprd sync --pipeline bikehire --log
```

Sync keeps reading the source until you stop it with Ctrl-C or your process manager. Run it under systemd, Docker, or Kubernetes with a restart policy; after a restart it resumes from the last committed offset.

### Structured output for monitoring

```bash
skipprd sync --pipeline bikehire --once --output json
```

Each line is one event:

| Event | When | Useful fields |
|---|---|---|
| `sync_start` | The run begins. | `pipeline`, `run_id` |
| `namespace_discovered` | A new namespace appears during the run. | `namespace`, `field_count` |
| `sync_status` | Every 10 seconds while the run is active. | `total_rows` (records read), `bytes`, `rows_written`, `uploads_in_flight`, `elapsed_ms` |
| `sync_error` | The source failed. | `error` |
| `sync_complete` | The run finished successfully. | `namespaces_synced`, `total_rows` (rows written), `elapsed_ms` |

Every event also has `event`, `phase`, and `timestamp`, plus extra context fields you can ignore. A trimmed `sync_complete` event:

```json
{"event":"sync_complete","phase":"complete","pipeline":"bikehire","namespaces_synced":1,"total_rows":48213,"elapsed_ms":21874,"timestamp":"2026-10-09T14:02:11.204518+00:00"}
```

### Syncing every pipeline

Leave out `--pipeline` (and `PIPELINE_NAME`) to sync every pipeline in `skippr.yml`, one after another:

```bash
skipprd sync --once --output text
```

In this mode each pipeline runs at most once per `sync_frequency_seconds` (default 900). A pipeline that ran more recently is skipped, and the log says `Pipeline '<name>' last ran N seconds ago, skipping`. Without `--once`, Skipprd checks again every 10 seconds.

### Use a dedicated config

```bash
skipprd --config /etc/skipprd/skippr.yml sync --pipeline bikehire --once
```

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Exits `1` with little or no output | The error is written to the logs. | Rerun with `--log`, or read `logs/skipprd.<date>.log` (in `DATA_DIR` if set, otherwise the current directory). |
| `sync --once` without `--pipeline` finishes but syncs nothing | Every pipeline ran within its `sync_frequency_seconds`. | Pass `--pipeline <name>` to run one pipeline now, or lower `sync_frequency_seconds`. |
| `[skippr] config failed: ... that environment variable is not set` | A `${NAME}` secret reference has no value. | `export NAME=...`, or put it in a `.env` file next to `skippr.yml`. |
| `pipeline writer lease unavailable` | Another `sync` is already writing this pipeline. | Let the other run finish, or stop it. Do not run two syncs of the same pipeline at once. |
| `WAL_STORAGE=clustered does not support sync --once` | Clustered mode is for long-running syncs. | Drop `--once`, or use `--wal-storage disk`. |
| Logs show `Pipeline '<name>' disabled, skipping.` | The pipeline was disabled. | Run `skipprd query --sql "ENABLE PIPELINE <name>"`. |
| No data in the destination after a successful run | The pipeline has no `data_sink`, so data stays in the WAL. | Add a destination with [`skipprd connect data-sink`](/cli/connect), or query the WAL with `skipprd query`. |

For crash recovery, deadletters, and other run-time issues, see [Troubleshooting](/operations/troubleshooting).

## Next steps

- [skipprd query](/cli/query)
- [Exactly-once delivery](/concepts/exactly-once)
- [WAL and buffering](/configuration/buffering)
- [Logging](/operations/logging)
