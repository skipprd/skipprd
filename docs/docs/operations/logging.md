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
3. Uploaded row counts match what Skipprd expected, and no partitions were quarantined.
4. A query against the destination returns the rows you expected.

Typical lines on a healthy pass, in order:

| Line (search for) | Meaning |
|---|---|
| `Syncing pipeline:` | The named pipeline started |
| `Discovered new namespace:` / `Discovered new field:` | First-seen table or column — expected on the first run and after a schema change |
| `Uploaded` + `parquet` + `rows=` | A file landed in the destination |
| `Pipeline sync complete` | The run finished |

On shutdown you may also see a drain line such as `Finalising: compactor drained and stopped`. That is the process finishing in-flight writes. The success marker you operate on is still `Pipeline sync complete` plus exit 0.

## When to treat the run as failed

| Line (search for) | Meaning | What you do |
|---|---|---|
| `Finalising: compactor drain/stop did not complete cleanly` | In-flight writes did not finish | Treat counts as untrusted. Re-run the same command. |
| `integrity check mismatch` or `quarantined_parts` greater than 0 | Uploaded rows do not match, or a slice could not be read | Compare destination counts to the source. See [Troubleshooting](/operations/troubleshooting). |
| `Pipeline '<name>' not found` | No schema for that pipeline | Run `skipprd discover --pipeline <name>` first. |
| `TABLE_NOT_FOUND` | The warehouse table is missing | Discover succeeded? Then re-run sync so the destination can create the table. |
| Non-zero exit | The run said it failed | Re-run. Committed WAL batches are retried. |

Throughput lines (`Messages per Min`, `Uploads total`) are trends, not a pass/fail signal.

## Chaos testing

`SKIPPR_CHAOS_MODE=yes` kills the process on purpose. You should see a SIGKILL, then a later run that recovers. Leave it off in production. See [Advanced settings](/configuration/advanced).

## Next steps

- [Troubleshooting](/operations/troubleshooting)
- [Exactly-once delivery](/concepts/exactly-once)
- [skipprd sync](/cli/sync)
