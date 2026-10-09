---
title: Exactly-once delivery
description: Learn what Skipprd guarantees when a sync crashes or retries, which destinations write each row exactly once, and how to plan for the rest.
---

# Exactly-once delivery

Skipprd commits every batch to its write-ahead log (WAL) before it writes to your destination. After a crash or a failed write, it retries from the WAL. Whether a retry can ever produce a duplicate row depends on the destination. This page tells you what you get with each one.

## What is guaranteed

Once Skipprd has committed a batch to the WAL:

- **It is not lost.** A process crash, including `kill -9`, does not lose committed batches. The next run writes them to the destination.
- **It is not read again from the source.** Skipprd only advances the source position after the commit, and resumes from that position on restart. Sources with a refresh window, such as Google Analytics, re-read recent days on purpose and replace them; see [How sources land](/concepts/source-landing-semantics).
- **It is written once** to destinations that support exactly-once writes. Each write carries a stable identity, so a retried write that already landed is recognised and skipped.

The guarantee starts at the WAL commit. A pull source (files, databases, APIs, Kafka) re-reads anything it had not committed, so nothing is lost there. Some push sources, such as the [HTTP server](/connectors/inputs/http_server) source, accept a request before Skipprd commits it. If the process crashes in that window, the request is lost; have senders retry failed requests and make your downstream tolerant of the resulting repeats.

## What each destination guarantees

| Guarantee on retry | Destinations | What you see |
|---|---|---|
| Exactly once | SkipprLake, Athena Iceberg, DuckDB, Snowflake, BigQuery, PostgreSQL, Redshift, Databricks, Synapse, MotherDuck | Each committed row lands once, even if Skipprd retries the write. |
| Overwrite on retry | Athena, S3, GCS, Azure Blob, Local file, SFTP | A retry rewrites the same file at the same path, so the final set of files holds each row once. |
| At least once | ClickHouse, AMQP | A retry can write a batch again. Deduplicate downstream, for example on a primary key. |
| None | Stdout | Output is not retried. Use it for debugging only. |

If duplicates matter, choose a destination from the "Exactly once" row. For change data capture, see [CDC guarantees](/cdc/guarantees): applying updates and deletes exactly once needs a CDC-capable source as well as the right destination.

## What happens after a crash

You do not need to do anything special. Run the same command again with the same `DATA_DIR`:

```bash
skipprd sync --pipeline bikehire --once --log
```

On start, Skipprd:

1. Finds the batches committed to the WAL that have not landed yet.
2. Restores the source position from the committed data, so the source resumes where it left off.
3. Writes the outstanding batches to your destination.
4. Continues reading the source.

If a run cannot finish writing its committed batches before it exits, it exits non-zero. Treat a non-zero exit as "retry later", not "data lost": the batches stay in the WAL for the next run.

## What can break the guarantee

The guarantee depends on the WAL and the resume position surviving. Watch for these:

- **Losing `DATA_DIR`.** With the default local WAL, `DATA_DIR` holds both. A fresh disk means Skipprd re-reads the source from the beginning. On ephemeral compute, use `WAL_STORAGE=s3` and a DynamoDB state store instead. See [WAL and buffering](/configuration/buffering) and [State store](/configuration/skippr-store).
- **Running `RESET PIPELINE`.** It deliberately deletes the WAL and the resume position. The next sync starts from scratch, which duplicates rows in append-only tables unless you clear them too.
- **Two copies of one pipeline.** Run one `skipprd sync` per pipeline at a time, always against the same `DATA_DIR`. Two copies with separate state each keep their own resume position and both write to the destination.

### Clustered mode

`WAL_STORAGE=clustered` keeps a synchronous second copy of the WAL on a peer, so a single machine failure loses no committed data. Clustered mode only accepts destinations whose retries cannot duplicate rows: the exactly-once and overwrite-on-retry destinations above. ClickHouse, AMQP, and Stdout are rejected at startup.

## Test recovery yourself

Chaos mode kills the process with `SIGKILL` at random points during a sync, so you can prove recovery against your own pipeline before you rely on it. Use it in a test environment only.

::: code-group

```python [Python]
from skippr import Config, Pipeline

cfg = Config.discover()
src = cfg.get_data_source("sample")
sink = cfg.get_data_sink("warehouse")
cfg.pipeline("bikehire", Pipeline(data_source=src, data_sink=sink, chaos_mode=True))
cfg.save()
```

```bash [CLI]
export SKIPPR_CHAOS_MODE=yes
skipprd sync --pipeline bikehire --log
```

```yaml [YAML]
pipelines:
  bikehire:
    data_source: data_sources.sample
    data_sink: data_sinks.warehouse
    chaos_mode: true
```

:::

Restart the sync after each kill, then compare row counts between the source and the destination.

## Next steps

- [How Skipprd works](/concepts/how-it-works): the pipeline lifecycle and where state lives.
- [CDC guarantees](/cdc/guarantees): exactly-once updates and deletes.
- [WAL and buffering](/configuration/buffering): pick a WAL backend.
- [Destinations](/configuration/output): configure a destination.
