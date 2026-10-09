---
title: Troubleshooting
description: Fix the errors operators hit first — missing pipelines, missing warehouse tables, credentials, deadletters, and a sync that did not finish cleanly.
---

# Troubleshooting

Start here when a command exits non-zero or the destination does not have the rows you expected. Re-run the same command after you fix the cause. Skipprd retries committed batches from the write-ahead log; you do not rewind the source by hand.

## `Pipeline '<name>' not found`

**Cause:** Skipprd has no schema for that pipeline yet.

**Fix:** Discover first, then sync:

```bash
skipprd discover --pipeline payments --log
skipprd sync --pipeline payments --once --log
```

Check the name matches `pipelines:` in `skippr.yml`.

## `TABLE_NOT_FOUND`

**Cause:** The destination table does not exist yet. Common on the first sync, or when discover never finished.

**Fix:** Confirm discover completed and the destination is paired with its schema sink (Athena needs [Glue](/connectors/schema_sinks/glue)). Then run sync again so tables can be created.

## Destination credential errors

**Cause:** The warehouse or object store rejected the caller.

**Fix:** Export the variables your connector page names (for AWS: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_DEFAULT_REGION`, or an instance role). Run the command in the same shell. `skipprd doctor` reports missing engine settings.

## Skipprd could not download a connector

The log line starts `Skippr could not find a published runtime`.

**Cause:** Skipprd could not download that source or destination. The name is wrong, the pinned `version` does not exist, or the machine cannot reach `install.skippr.io`.

**Fix:**

1. Check the connector name and casing in `skippr.yml` against the [connector catalog](/connectors/).
2. Remove `version:` if you meant “latest”.
3. Confirm outbound HTTPS to `install.skippr.io`.
4. On a first run, wait — Skipprd caches the download under `~/.skippr/runtime_plugins`.

## The run did not finish cleanly

Look for `Finalising: compactor drain/stop did not complete cleanly` or `integrity check mismatch`.

**Cause:** Skipprd exited before in-flight writes settled, or uploaded counts do not match.

**Fix:** Treat that run's counts as untrusted. Re-run:

```bash
skipprd sync --pipeline payments --once --log
```

The next run recovers from the WAL. Confirm `Pipeline sync complete` and that destination counts match the source. If a summary reports `quarantined_parts` greater than 0, a slice failed to read — check source files or objects for corruption, then re-run.

## Deadletters

`Discarded <n> deadletter records because no deadletter sink is configured` means rows failed validation and were dropped. Add a `deadletter_sink` if you want to inspect them. See [Deadletters](/concepts/deadletters).

`Deadletter id=… ns=… err=…` is one failed record. Query the deadletter destination and fix the payload or the schema.

## Recovery after a crash

You do not run a special recover command.

1. The WAL still holds committed batches.
2. Resume positions say how far the source got.
3. The next `skipprd sync` writes remaining batches and continues the source.

Use the same `DATA_DIR` (or the same S3 WAL and state store). If you deleted them, Skipprd re-reads the source and append-only destinations can duplicate rows. See [WAL and buffering](/configuration/buffering).

## Next steps

- [Logging](/operations/logging)
- [Deadletters](/concepts/deadletters)
- [Exactly-once delivery](/concepts/exactly-once)
- [skipprd doctor](/cli/doctor)
