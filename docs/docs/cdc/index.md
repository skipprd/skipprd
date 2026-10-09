---
title: Change data capture
description: Keep a warehouse table in step with a live database. Choose snapshot, continuous CDC, or both, then resume safely after a crash.
---

# Change data capture

Use CDC when you want the destination table to match the source as it is now — including updates and deletes — not a one-off copy.

Skipprd reads changes from the source (for example PostgreSQL logical replication), commits each batch to the write-ahead log, then applies it to your destination. The next run resumes from the last committed position. You do not replay the source log yourself.

## Choose a mode

Set `cdc_mode` on the source. The common database sources (PostgreSQL, MySQL, SQL Server, MongoDB, DynamoDB) support it.

| Mode | What one sync does | Use when |
|---|---|---|
| `snapshot` | Copies each selected table once. Later runs skip tables already loaded. | A one-off migration or backfill. New and changed rows are not picked up. |
| `snapshot_then_cdc` | Copies the tables, then streams inserts, updates, and deletes. | You want a full copy, then a live destination. |
| `cdc_only` | Streams changes from now on, with no initial copy. | The destination already has the history, or you only care about new activity. |

Deletes only land when the mode includes CDC. A `snapshot` source never sends a delete.

## What you need at the source

Each database has its own setup. In general you will:

1. Grant Skipprd a read role (and replication rights for CDC).
2. Turn on the source's change stream (logical replication, binlog, change streams).
3. Give every replicated table a primary key, or the source cannot describe updates and deletes.
4. Drop the replication slot or stream when you retire the pipeline, or the source keeps change data and its disk grows.

See the source page for the exact SQL or console steps: [PostgreSQL](/connectors/inputs/postgres), [MySQL](/connectors/inputs/mysql), [SQL Server](/connectors/inputs/mssql), [MongoDB](/connectors/inputs/mongodb), [DynamoDB](/connectors/inputs/dynamodb).

## What lands in the destination

The destination table should match the source's current rows for the tables you replicate. Intermediate versions of a row are not preserved. If a row is inserted and updated twice before Skipprd writes, the destination gets the latest version.

Pick a destination that can apply updates and deletes. Warehouses that only append will grow duplicates unless you model that downstream. See [CDC guarantees](/cdc/guarantees) and [Exactly-once delivery](/concepts/exactly-once).

## After a crash

Run the same `skipprd sync` command again with the same `DATA_DIR`. Skipprd resumes from the last committed position. You do not rewind the source or clear the destination.

## Next steps

- [CDC guarantees](/cdc/guarantees) — order, deletes, and retries
- [PostgreSQL source](/connectors/inputs/postgres) — snapshot and logical replication setup
- [Exactly-once delivery](/concepts/exactly-once)
