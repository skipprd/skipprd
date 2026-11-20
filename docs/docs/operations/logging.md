---
title: Logging
description: Turn on Skipprd logs, recognise a successful sync, and know which lines mean you should treat the run as failed.
---

# Logging

Add `--log` to any command. The default level is `info`. Use `--log debug` only when you are chasing a specific failure — it is noisy.

```bash
skipprd sync --pipeline payments --once --log
```

Read logs in the terminal or the collector you already use. Skipprd writes structured lines you can grep.

## A successful run

Treat a `sync --once` as good when all of these are true:

1. The process exits 0.
2. The log contains `Pipeline sync complete`.
3. `Finalising: compactor drained and stopped` exists (when the pipeline has a `data_sink`).
4. `Compactor: summary` has `uploaded_rows == expected_uploaded_rows` and `quarantined_parts=0`.
5. A query against the destination returns the rows you expected.

WAL-only pipelines (`No data_sink for pipeline; WAL is the dataset`) skip compaction and destination uploads. For those, operate on `Pipeline sync complete` plus exit 0.

## Log event map

Use this as a quick lookup: **log event -> what it means -> what to do**.

### Green path events

| Log event (pattern) | Meaning | Action |
|---|---|---|
| `Initializing data directories...` | Runtime startup | None |
| `Initialized data directories at: ...` | `DATA_DIR` ready | None |
| `Persisted config: .../config/...json` | Run config written to Skippr storage | None |
| `get_metadata: pipeline='...' schema=skippr-store:schema/...` | Schema lookup in Skippr Store | None |
| `Found existing Skippr metadata` | Schema family already present | None |
| `imported metadata.json into schema family` | Leftover `metadata.json` imported once | Expected on first Store cutover |
| `Updated pipeline schema family` | Schema persisted to Skippr Store | Should appear during discover/evolution |
| `WAL catalog hydrate begin` / `WAL catalog hydrate complete` | Catalog rows registered into the live WAL | Check `live_segments` / `segments` rise after restart/chaos |
| `WAL catalog redo_acked begin` | Acked compaction rows replayed | None |
| `WAL catalog republish begin` / `WAL catalog republish complete` | Offsets republished from the catalog | Expected on startup |
| `WAL startup complete` | WAL import, hydrate, and republish finished | Required before ingest |
| `Output plugin: <name>` | Destination selected | None |
| `No data_sink for pipeline; WAL is the dataset` | WAL-only pipeline; compaction skipped | Do not expect upload/summary equality |
| `Syncing pipeline` | Named pipeline started (`pipeline=`, `pipeline.type=`) | None |
| `stream processor started` | Stream transforms/MVs active | None |
| `Syncing bucket: <bucket>, prefix: <prefix>` | S3 source listing started | None |
| `Starting stream pipeline (...)` | S3 ingest worker pipeline active | None |
| `Queueing ... ingest tasks ...` | Batches entering the worker queue | Watch if queue grows without progress |
| `Discovered new namespace: ...` | New table seen in input | Expected on first run |
| `Discovered new field: ...` | Schema evolution detected | Expected with changing payloads |
| `Schema discovery complete, writing metadata to Skippr` | Discover finished sampling | Discover path only |
| `Schema sync: updating namespace ... version ...` | Destination catalog DDL in flight | None |
| `Schema sync: synced namespace ... version ...` | Destination schema published | Should follow evolution |
| `Uploaded s3://... to S3 (rows=..., bytes=..., namespace=...)` | Destination object written | Warehouse sinks log their own apply line instead |
| `Messages per Min: ...` | Throughput snapshot | Trend only |
| `Uploads total: ..., inflight: ..., avg latency: ...` | Output pressure telemetry | Investigate if inflight/latency spike |
| `Targets - upload: ..., wal: ..., s3_download: ...` | Autotuned concurrency | Trend only |
| `WAL writer: queue=... pending=...` | WAL persist pressure | Investigate if pending grows without persist |
| `Reached end of source data` | Source plugin finished (`--once` or bounded source) | Must be followed by finalising |
| `Finalising: draining stream processor` | Stream processor shutdown started | None |
| `Finalising: draining and stopping compactor` | End-of-run drain started | Must be followed by success line |
| `Compactor drain: elapsed=...` | Drain progress heartbeat | Watch `no progress` while backlog stays high |
| `Finalising: compactor drained and stopped` | End-of-run drain complete | Required for trustworthy completion when a sink is configured |
| `Finalising: draining schema sync worker` / `Finalising: schema sync worker drained` | Catalog DDL worker settled | None |
| `Finalising: draining durable catalog outbox` | Iceberg/Glue catalog commits flushing | Must be followed by finished or failed |
| `Finalising: durable catalog outbox drain finished` | Catalog commits settled | Required before treating Iceberg/Athena as queryable |
| `Compactor: summary uploaded_rows=X wal_write_rows=Y expected_deadletters=Z expected_uploaded_rows=W quarantined_parts=Q` | Final integrity snapshot | Expect `X == W` (`W` is `Y + Z`) and `Q == 0` |
| `Pipeline sync complete` | Run completed | Final success marker |

### High-priority warnings and errors

| Log event (pattern) | Meaning | Action |
|---|---|---|
| `Finalising: stream processor drain did not complete cleanly` | Stream processor still in flight at shutdown | Treat stream state as untrusted. Re-run. |
| `Finalising: compactor drain/stop did not complete cleanly` | Shutdown safety invariant failed | Treat the run as failed/untrusted. Re-run. |
| `Finalising: durable catalog outbox drain failed` | Catalog commits did not settle | Treat destination catalog as untrusted. Re-run. |
| `Compactor: integrity check mismatch` | `uploaded_rows != wal_write_rows + expected_deadletters` or `quarantined_parts > 0` | Treat as a correctness risk; validate destination counts |
| `quarantined_parts > 0` (in summary) | Partition(s) quarantined after a read/parse failure | Investigate WAL/object integrity |
| `hydrate kept unreadable catalog row` | A catalog segment payload could not be registered | Integrity check before trusting resume |
| `DATA_DIR pause escape found ... unreadable committed WAL segment` | Pause recovery found corrupt WAL | Stop. Do not resume ingest until WAL is intact |
| `Deadlettered ... records into WAL` | Failed records written to the deadletter sink | Inspect the deadletter table |
| `Discarded ... deadletter records because no deadletter sink is configured` | Failed records dropped | Add a `deadletter_sink` if you need to inspect them |
| `CDC update rejected: no before-image` | CDC update had no before-image; no WAL row | Fix source image settings (`REPLICA IDENTITY FULL` / `binlog_row_image=FULL`) |
| `Skippr could not find a published runtime` | Connector download/name/version failed | See [Troubleshooting](/operations/troubleshooting) |
| `TABLE_NOT_FOUND` | Destination table missing | Tables exist after the first compact, not during ingest |
| `Pipeline '<name>' not found` | No schema for that pipeline (query/SQL) | Run `skipprd discover --pipeline <name>` first |
| `Pipeline '<name>' disabled, skipping.` | Pipeline is toggled off | Expected; not a failure |

Throughput lines (`Messages per Min`, `Uploads total`, `WAL writer`, `Targets`) are trends, not a pass/fail signal.

## Fast trust checklist

A run is operationally green if all are true:

- Exit 0
- `Pipeline sync complete`
- `Finalising: compactor drained and stopped` (skip when WAL-only)
- `Finalising: durable catalog outbox drain finished` when that line appears
- `Compactor: summary` `uploaded_rows == expected_uploaded_rows` and `quarantined_parts=0`
- Downstream query can read the destination table(s)

`--log perf` keeps `skipprd::perf` PERF_EVENT lines and turns on COPY Instant timers (`skippr.perf_profile`). Use it for skippr-perf and local COPY benches, not production.

## Chaos testing

`SKIPPR_CHAOS_MODE=yes` kills the process on purpose. You should see `Chaos mode throwing a random exit...`, then a SIGKILL (exit 137), then a later run that recovers. The recovery run logs `WAL catalog hydrate complete` / `WAL catalog republish complete` with live segments from the interrupted write. Leave chaos off in production. See [Advanced settings](/configuration/advanced).

## Next steps

- [Troubleshooting](/operations/troubleshooting)
- [Exactly-once delivery](/concepts/exactly-once)
- [skipprd sync](/cli/sync)
