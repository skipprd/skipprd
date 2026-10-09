---
title: WAL and buffering
description: Keep synced data safe across crashes. Choose a local disk, S3, or a multi-node write-ahead log, and decide how large a batch Skipprd holds in memory.
---

# WAL and buffering

Every batch Skipprd reads is written to a **write-ahead log (WAL)** before it reaches your destination. If the process dies, the next run recovers from the WAL and continues. You choose where that log lives and how large a batch sits in memory before Skipprd flushes it.

See [How Skipprd works](/concepts/how-it-works) for the pipeline lifecycle. This page is the production switch.

## Where the WAL lives

Set `WAL_STORAGE`. There is no `skippr.yml` field for this — it is an environment variable. An unknown value fails startup.

| Value | Where segments live | Use when |
|---|---|---|
| `disk` (default) | `DATA_DIR` on this machine | A long-lived host with a persistent disk |
| `s3` | `SKIPPR_WAL_S3_BUCKET`, or `SKIPPR_S3_BUCKET` if that is unset | Containers, Lambda, or any host you can throw away |
| `clustered` | Local disk plus a second live process, with resume state in [SkipprStore](/configuration/skippr-store) | More than one machine must survive a node loss |

### Local disk

```bash
export WAL_STORAGE=disk
export DATA_DIR=/var/lib/skipprd
skipprd sync --pipeline payments --once
```

Reuse the same `DATA_DIR` on every run. If you start on a fresh disk, Skipprd has no WAL and no resume position.

### Object storage

```bash
export WAL_STORAGE=s3
export SKIPPR_WAL_S3_BUCKET=acme-skipprd-wal
export SKIPPR_STORE_TYPE=dynamodb
export SKIPPR_STORE_NAME=acme-skipprd-offsets
skipprd sync --pipeline payments --once
```

Give the WAL its own bucket so pipeline state is not mixed with lake files. Pair S3 WAL with DynamoDB (or Cloud Tables) so resume positions survive when the machine is gone. See [State store](/configuration/skippr-store).

### More than one node

`WAL_STORAGE=clustered` keeps two durable copies and stores leases and resume positions in SkipprStore. You need three live `skipprd sync` processes to keep writing after one node fails. `sync --once` and `discover` are rejected in this mode — `sync` is the long-lived node. `skipprd query` never takes a write lock.

```bash
export WAL_STORAGE=clustered
export SKIPPR_STORE_NAME=acme-skipprd-offsets
skipprd sync --pipeline payments
```

## How large a batch is

Skipprd holds records in memory and flushes when either limit is hit.

| Variable | Default | Meaning |
|---|---|---|
| `BUFFER_THRESHOLD_BYTES` | `10485760` (10 MB) | Flush when the in-memory batch reaches this size |
| `BUFFER_THRESHOLD_SECONDS` | `60` | Flush when the batch is this old, even if it is smaller |

Leave the defaults unless you have a reason. A larger byte threshold means fewer, bigger writes to the destination. A smaller second threshold means rows appear sooner.

These optional overrides rarely need a change:

| Variable | Default | Meaning |
|---|---|---|
| `WAL_BYTES_PER_FILE` | Follows `BUFFER_THRESHOLD_BYTES`, clamped to 4–64 MiB | Target size of each WAL file |
| `WAL_MAX_DELAY_SECONDS` | `60` | Flush a WAL file that has not reached the size target |

## What to back up

| Mode | Back up |
|---|---|
| `disk` | `DATA_DIR` and `skippr.yml` |
| `s3` | The WAL bucket and the SkipprStore table |
| `clustered` | The SkipprStore table and the disks on the live nodes |

If the WAL is gone, Skipprd re-reads the source. Append-only destinations can then duplicate rows.

## Next steps

- [State store](/configuration/skippr-store)
- [How Skipprd works](/concepts/how-it-works)
- [Exactly-once delivery](/concepts/exactly-once)
- [Advanced settings](/configuration/advanced)
