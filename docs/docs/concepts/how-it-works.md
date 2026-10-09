---
title: How Skipprd works
description: Follow a pipeline from discover to query, learn what the write-ahead log guarantees, and know which state to keep and back up.
---

# How Skipprd works

You describe a **pipeline** in `skippr.yml`: one source, and optionally one destination. Then you run four commands: discover the shape of the data, review the schema, sync the rows, and query them. Every batch is written to Skipprd's write-ahead log (WAL) first. A crash retries from the WAL. Whether a retry can duplicate a row depends on the destination — see [Exactly-once delivery](/concepts/exactly-once).

```text
                ┌──────────────────────── skipprd ────────────────────────┐
  data source ──▶  read batch ──▶ commit to WAL ──▶ write to destination   ──▶ destination
                │                      │                     │            │
                │                      ▼                     ▼            │
                │              resume position        deadletter sink     │
                │              (state store)          (optional)          │
                └──────────────────────────────────────────────────────────┘
```

## The pipeline lifecycle

### 1. Discover

```bash
skipprd discover --pipeline bikehire --log
```

Skipprd connects to the source, samples records, and infers field names, nesting, and types. It saves the result as the pipeline's schema. Discover never loads rows into your destination, so it is safe to run against production sources. See [Schema discovery and evolution](/concepts/schema).

### 2. Review the schema

```bash
skipprd schema --pipeline bikehire
```

Prints the discovered contract: fields, types, nesting, and namespaces (one namespace becomes one table). Check it before the first sync so table and column names in your warehouse are what you expect.

### 3. Sync

```bash
skipprd sync --pipeline bikehire --once --log
```

For each pass, Skipprd:

1. Reads a batch from the source.
2. Commits the batch to the WAL. From this moment the batch is durable.
3. Records the source position it has reached, so the next run resumes there.
4. Writes committed batches to your destination and updates destination tables when the schema changes.
5. Deletes WAL data your destination has acknowledged.

`--once` runs one pass and exits, which suits cron, CI, and orchestrators. Without it, `skipprd sync` keeps running and starts a new pass every `sync_frequency_seconds` (default 900).

### 4. Query

Query the tables in your warehouse with its own SQL client. For Iceberg destinations (SkipprLake, Athena Iceberg, DuckDB), you can also query from the machine running Skipprd with [`skipprd query`](/cli/query). That view includes rows still in the WAL that have not landed yet. See [Datalake](/concepts/datalake).

## What the WAL guarantees

The WAL is the durable buffer between your source and your destination. Skipprd commits each batch there before it writes anywhere else, and only advances the source position after the commit.

This matters to you in three ways:

- **Crashes are safe.** If the process dies, including `kill -9`, the next run recovers committed batches from the WAL and resumes the source from the last committed position. Re-run the same command; there is no manual recovery step.
- **Retries do not duplicate rows** in destinations that support exactly-once writes. Skipprd identifies each write so a retried write is not applied twice. Which destinations qualify is listed in [Exactly-once delivery](/concepts/exactly-once).
- **A failed run says so.** If Skipprd cannot finish writing committed batches before it exits, the run exits non-zero. The next run picks up from the WAL.

The guarantee starts when a batch is committed. Data a source has handed over but Skipprd has not yet committed is covered only if the source can replay it, which pull sources such as files, databases, and APIs do.

### With a destination

Set `data_sink` on the pipeline and Skipprd writes committed batches to that destination, then reclaims the WAL space. The WAL stays small: it holds only data that has not landed yet.

### Without a destination

`data_sink` is optional. Without it, the WAL *is* the dataset: Skipprd keeps every batch and never reclaims it. Use this to explore a source before choosing a warehouse. Read the data with `skipprd query` or Python `Session.query()` and `Session.df()`.

```yaml
pipelines:
  bikehire:
    data_source: data_sources.sample
```

Because nothing is reclaimed, watch disk use on long-running pipelines without a destination.

## Where state lives

Skipprd keeps four kinds of state. Know where each one lives before you run a pipeline in production.

| State | Default location | What to do |
|---|---|---|
| `skippr.yml` | Your project directory | Keep it in version control. Secrets stay in the environment as `${NAME}` references. |
| WAL and resume positions | `DATA_DIR` (default `./data`), one subdirectory per pipeline | Put `DATA_DIR` on a persistent disk and reuse the same path on every run. |
| Pipeline schema | The S3 bucket in `skippr.skippr_s3_bucket`, or under `DATA_DIR` with `skipprd_el_storage_mode: local` | Keep the bucket. Losing it means running discover again. |
| Connector cache | `~/.skippr/runtime_plugins` | Nothing. Skipprd downloads a connector again if the cache is empty. |

Set the data directory with the `DATA_DIR` environment variable or per pipeline with `data_dir` in `skippr.yml`.

::: warning Keep DATA_DIR
With the default local WAL, `DATA_DIR` holds data that may not have reached your destination yet, plus the position Skipprd resumes from. If you delete it or start on a fresh disk, Skipprd loses those batches and reads the source again from the beginning, which can duplicate rows in append-only destinations.
:::

If your compute is ephemeral (containers, Lambda, autoscaled hosts), move the WAL to S3 with `WAL_STORAGE=s3` and the state store to DynamoDB. Then no state lives on the machine. See [WAL and buffering](/configuration/buffering) and [State store](/configuration/skippr-store).

Skipprd pauses reading when the disk holding `DATA_DIR` has less than 5 GB free or is 90% full, and resumes once usage drops back to 80%. A pipeline that stalls on a busy host is often waiting for disk space.

## How connectors are installed

Each source and destination is a connector Skipprd downloads for you. The first time a pipeline uses one, Skipprd:

1. Looks up the latest version at `install.skippr.io`.
2. Downloads it into the connector cache (`~/.skippr/runtime_plugins`).
3. Reuses that copy on later runs of the same version.

The machine needs outbound HTTPS to `install.skippr.io`. To put the cache on a persistent volume, set `SKIPPR_RUNTIME_PLUGIN_DIR` to that path.

By default Skipprd uses the latest version of each connector. To pin a connector, add `version` to its entry in `skippr.yml`:

```yaml
data_sources:
  sample:
    S3:
      version: 1.2.3
      s3_bucket: skippr-public-sample-data
      s3_prefix: bike-hire
```

## Next steps

- [Install Skipprd](/getting-started/install) and run the [S3 to Athena quickstart](/getting-started/quickstart).
- [Exactly-once delivery](/concepts/exactly-once): what each destination guarantees on retry.
- [Schema discovery and evolution](/concepts/schema): how types are inferred and how changes land.
- [WAL and buffering](/configuration/buffering): choose a WAL backend for production.
- [skippr.yml](/configuration/skippr-yml): the full project file.
