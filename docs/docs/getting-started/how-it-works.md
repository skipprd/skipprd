---
title: Pipeline flow
description: Skipprd discover infers schema; Skipprd schema prints it; Skipprd sync writes through the WAL into your destination.
---

# Pipeline flow

A pipeline is a named path from a source plugin to a sink plugin in `skippr.yml`. Skipprd loads that file, discovers the source shape, then moves rows through a write-ahead log into the destination you configured.

## Prerequisites

- `skipprd` on `PATH` ([Install](install.md))
- A `skippr.yml` with at least one pipeline, `data_source`, and `data_sink`

## Discover

```bash
skipprd discover --pipeline bikehire --log
```

The engine connects to the source, samples records, and infers nested fields and types. The schema is persisted as pipeline metadata (S3 when `skippr_s3_bucket` is set). Discover does not write to the destination.

Type mapping is deterministic. Ingestion correctness does not depend on a generated model.

## Schema

```bash
skipprd schema --pipeline bikehire
```

Prints the discovered contract: field names, types, nesting, and namespaces.

## Sync

```bash
skipprd sync --pipeline bikehire --once --log
```

1. Resolve runtime source, sink, and schema plugins from `install.skippr.io`.
2. Read from the source.
3. Commit batches to the WAL. Visible committed WAL state is the durable ingest boundary.
4. Compact committed work and write to the destination with replay-safe compaction ids.
5. Materialize resume offsets from WAL-visible progress.

`--once` completes a single pass and exits. Omit it to keep reading. On crash, recovery starts from committed WAL state, not from in-memory progress. See [exactly-once](/concepts/exactly-once).

Supported CDC paths converge on the table as it should be — order tokens, tombstones, warehouse reconciliation — not a raw log dump. See [CDC guarantees](/cdc/guarantees).

After sync, run SQL in the destination (Snowflake, Athena, PostgreSQL, BigQuery).

## Data path

Row-level source data is read on the machine running `skipprd` and written to your destination. Credentials stay in the environment, not in git.

## Troubleshooting

- **Discover succeeds, sync writes nothing** — confirm `--once` completed without deadletters and that the sink credentials can write.
- **Schema drift / type conflict** — incompatible type changes go to the [deadletter](/concepts/deadletters) path; valid rows continue.
- **Resume looks wrong after a kill** — offsets are a cache of WAL-visible progress. Recovery replays committed WAL segments first.
