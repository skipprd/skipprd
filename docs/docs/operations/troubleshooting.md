# Troubleshooting

## Common errors

### `Pipeline '<name>' not found`

**Cause:** `ENABLE PIPELINE` or `DISABLE PIPELINE` was called before the pipeline metadata exists.

**Fix:** Run `skippr-el discover --pipeline <name>` first to create the metadata, then enable the pipeline.

### `TABLE_NOT_FOUND`

**Cause:** The Glue table hasn't been created yet. This happens when `sync` runs before a schema has been discovered and synced to the destination.

**Fix:** Ensure `discover` has been run and completed successfully. Then run `sync`, which will create the Glue database and table on startup.

### `Skippr could not find a published runtime ... plugin named ...`

**Cause:** Runtime plugin discovery could not find a matching manifest in the latest published index, or a pinned `version` does not exist for that plugin.

**Fix:**
1. Check the plugin name and casing in the pipeline config
2. Remove the version pin if you intended to follow latest
3. Verify the plugin crate has been published and appears in `latest/manifest-index.json`
4. If you are testing local artifacts, switch to an explicit manifest override or `--local-runtime-manifest-dir` in the runtime e2e harness

### `skippr-el artifact unexpectedly contains runtime plugin binaries`

**Cause:** A host artifact directory contains `skippr-plugin-*` executables beside `skippr-el`. The runtime e2e harness rejects this because the host is not supposed to ship bundled plugins anymore.

**Fix:** Copy `skippr-el` into a clean temporary directory before running the harness, or point the harness at a release artifact directory that contains only the host binary.

### `Compactor: integrity check mismatch`

**Cause:** The number of uploaded rows doesn't match the expected count, or partitions were quarantined. This can indicate data loss or duplication.

**Fix:**
1. Check whether `quarantined_parts > 0` — if so, investigate the specific WAL segments or Parquet files for corruption
2. If `uploaded_rows != expected_msgs`, validate the actual data in Athena against the source
3. Consider running `RESET PIPELINE` and re-ingesting if the mismatch is confirmed

### `Finalising: compactor drain/stop did not complete cleanly`

**Cause:** The compactor couldn't finish processing all WAL segments before shutdown.

**Fix:** Treat this run as untrusted. The next run will recover from the WAL and re-process any incomplete segments. No data is lost, but the run's counts should not be trusted.

### `A Tokio 1.x context was found, but it is being shutdown`

**Cause:** Async work was still in progress when the Tokio runtime was torn down. This typically means a background task (like a multipart upload) was interrupted.

**Fix:** This is a shutdown sequencing issue. The next run will recover from the WAL. If this happens repeatedly, check that the compactor drain is completing before exit.

### AWS credential errors

**Cause:** Missing or invalid AWS credentials.

**Fix:** Ensure `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and `AWS_DEFAULT_REGION` are set. Alternatively, use an instance profile or IAM role.

### `Discarded <n> deadletter records because no deadletter sink is configured`

**Cause:** The pipeline produced deadletters but does not define a `deadletter_sink: deadletter_sinks.<name>`.

**Fix:** Add a deadletter sink if you want those records retained. Otherwise this message is informational.

### `Deadletter id=<id> ns=<namespace> err=<error>`

**Cause:** A record failed validation or schema matching during ingestion.

**Fix:** This is expected behaviour. Query the configured deadletter destination to inspect failures. See [Deadletters](../concepts/deadletters.md).

## Recovery after crash

Skippr is designed to recover automatically:

1. The WAL preserves all ingested data
2. The offsets database tracks what has been committed
3. On restart, the WAL is scanned, committed segments are skipped, and remaining segments are reprocessed

No manual intervention is needed. Verify recovery by checking that `uploaded_rows == expected_msgs` in the compactor summary.

## Resetting a pipeline

To re-ingest from scratch:

```bash
skippr-el query --sql "RESET PIPELINE my_pipeline"
```

This clears the offsets database and WAL for the pipeline. The next `sync` will start from the beginning of the source data.
