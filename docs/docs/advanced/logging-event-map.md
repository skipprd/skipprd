# Logging Event Map

Use this as a quick lookup: **log event -> what it means -> what to do**.

Skippr operationally runs as stateless compute: no clustering required, no scaling group orchestration required, and local disk WAL is optional when durable remote WAL storage is configured.

## Green path events

| Log event (pattern) | Meaning | Action |
|---|---|---|
| `Initializing data directories...` | Runtime startup | None |
| `Uploaded config to S3: .../config/...json` | Run config persisted | None |
| `get_metadata: tenant=... workspace=... pipeline=...` | Metadata lookup started | None |
| `Loaded metadata from S3 (entries=..., keys=[...])` | Existing metadata loaded | None |
| `Syncing pipeline: <name>` | Pipeline execution started | None |
| `Offset DB size: ...` | Offset store loaded | None |
| `WAL scan examined ...` | WAL replay/index pass completed | Check counts increase on restart/chaos |
| `Indexed ... Segment files ...` | Recoverable WAL segments indexed | None |
| `Committed ... offsets from .seg files` | Replay advanced offsets | None |
| `Output plugin: Athena` | Output target selected | None |
| `Syncing bucket: <bucket>, prefix: <prefix>` | Source connection active | None |
| `Starting stream pipeline (...)` | Ingest worker pipeline active | None |
| `Queueing ... ingest tasks ...` | Batches entering worker queue | Watch if queue grows without progress |
| `Discovered new namespace: ...` | New namespace seen in input | Expected on first run |
| `Discovered new field: ...` | Schema evolution detected | Expected with changing payloads |
| `Updated pipeline metadata in S3: .../metadata.json` | Metadata persisted | Should appear during discovery/evolution |
| `Uploaded ...parquet to S3 (rows=..., bytes=...)` | Output objects written successfully | None |
| `Messages per Min: ...` | Throughput snapshot | Trend only |
| `Uploads total: ..., inflight: ..., avg latency: ...` | Output pressure telemetry | Investigate if inflight/latency spike |
| `Targets - upload: ..., wal: ..., s3_download: ...` | Concurrency target state | Trend only |
| `Finalising: draining and stopping compactor` | End-of-run drain started | Must be followed by success line |
| `Finalising: compactor drained and stopped` | End-of-run drain complete | Required for trustworthy completion |
| `Finalising: Athena partition tasks drained` | Glue/Athena background tasks settled | Required before completion |
| `Compactor: summary uploaded_rows=X expected_msgs=Y quarantined_parts=Z` | Final integrity snapshot | Expect `X == Y` and `Z == 0` |
| `Pipeline sync complete` | Run completed | Final success marker |

## Chaos mode events (expected)

| Log event (pattern) | Meaning | Action |
|---|---|---|
| `Chaos mode throwing a random exit...` | Intentional kill injected | None |
| `Killed ...` + `exit 137` | SIGKILL occurred | Expected in chaos tests |
| `Chaos SIGKILL (exit 137) observed; continuing as expected` | Wrapper accepted chaos kill | None |
| Next run has higher `WAL scan examined ...` / `Indexed ...` | Recovery from interrupted run | Expected |

## High-priority warnings/errors

| Log event (pattern) | Meaning | Action |
|---|---|---|
| `Finalising: compactor drain/stop did not complete cleanly` | Shutdown safety invariant failed | Treat run as failed/untrusted |
| `A Tokio 1.x context was found, but it is being shutdown` | Async work still running at runtime teardown | Investigate shutdown sequencing immediately |
| `Compactor: integrity check mismatch ...` | Uploaded rows do not match expected or quarantined > 0 | Treat as correctness risk; validate output counts |
| `quarantined_parts > 0` (in summary) | Partition(s) quarantined due to read/parse issues | Investigate WAL/parquet integrity |
| `TABLE_NOT_FOUND ... awsdatacatalog.<db>.<table> ...` | Destination table missing/unavailable | Verify schema sync/catalog creation before Soda checks |
| `Pipeline '<name>' not found` | Enable/toggle called before metadata exists | Run discover first, then enable |

## Fast trust checklist

A run is operationally green if all are true:

- `Pipeline sync complete` exists
- `Finalising: compactor drained and stopped` exists
- `Compactor: summary uploaded_rows == expected_msgs`
- `quarantined_parts=0`
- no Tokio shutdown panic
- downstream query/Soda can read destination table
