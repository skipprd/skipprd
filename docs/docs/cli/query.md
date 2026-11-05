---
title: skipprd query
description: Run SQL against your pipelines from the terminal or a script, and manage pipelines and schemas with Skipprd SQL statements.
---

# skipprd query

`skipprd query` runs SQL against the data your pipelines have ingested. Each namespace is a table named `<pipeline>.<namespace>`, for example `bikehire.rides`. You query Skippr once: live stream plus history together. You do not pick a live view or a history view. What a table contains depends on the destination:

- **No destination, or a non-Iceberg destination** (Athena, Snowflake, Postgres, and others): the live data in the WAL. For the full history in a warehouse, query the warehouse itself.
- **Iceberg destination** (SkipprLake, AthenaIceberg, DuckDB): the Iceberg table combined with live data still in the WAL, so you see rows the moment they are ingested.

Use `query` to check a sync, explore data, script checks in CI, tail new results with `STREAM`, and run Skipprd's management statements (`ENABLE PIPELINE`, `SCHEMA DUMP`, `ALTER TABLE`, and others). Without `--sql` it opens an interactive prompt. How SELECT, STREAM, and Skippr Store pins fit together: [Stream SQL](/concepts/stream-sql).

## Usage

::: code-group

```python [Python]
import skippr

session = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
table = session.query("SELECT count(*) AS rides FROM bikehire.rides")
print(table.to_pylist())
```

```bash [CLI]
skipprd query --sql "<SQL>" [--plain] [--watch <seconds>] [--warmup] [--repeat <N>] [--config <path>] [--log [LEVEL]]
skipprd query                      # interactive prompt
```

:::

`query` reads every pipeline in `skippr.yml`; there is no `--pipeline` flag. Name tables in full.

## Options

| Flag | Default | Description |
|---|---|---|
| `-s`, `--sql <SQL>` | None | Statement to run. Without it, `query` opens an interactive `sql>` prompt; type `:q`, `:quit`, or `exit` to leave. |
| `--plain` | Off | Print one JSON document to stdout instead of the interactive table view: `{"header": [...], "rows": [[...]]}`. All values are strings. Use this in scripts. |
| `--watch <SECONDS>` | Off | Re-run a `SELECT` every N seconds in the interactive table view. Ignored with `--plain`. Not `STREAM`. |
| `--warmup` | Off | Open catalogs and run the statement once, then time the second collect. |
| `--repeat <N>` | `1` | Extra untimed collects after the measured one. For profiling. |
| `--config <PATH>` | `./skippr.yml` | Config file to read. |
| `--log [LEVEL]` | Off | Print logs to stderr. |
| `--wal-storage <MODE>` | `disk` | WAL backend: `disk` or `s3`. Same as `WAL_STORAGE`. |

See [CLI overview](/cli/overview) for the other global flags.

In the interactive table view, press Enter to run the SQL and `q` to quit. Outside `--plain`, the elapsed time is printed to stderr as `Query time: N seconds`.

## Examples

### Count rows for a script

```bash
skipprd query --plain --sql "SELECT count(*) AS rides FROM bikehire.rides"
```

```json
{"header":["rides"],"rows":[["48213"]]}
```

Parse it with `jq`:

```bash
skipprd query --plain --sql "SELECT count(*) AS rides FROM bikehire.rides" | jq -r '.rows[0][0]'
```

### Explore interactively

```bash
skipprd query --sql "SELECT station, count(*) AS rides FROM bikehire.rides GROUP BY station ORDER BY rides DESC LIMIT 10"
```

### Watch a table fill during a sync

```bash
skipprd query --sql "SELECT count(*) FROM bikehire.rides" --watch 5
```

### Stream query

`STREAM` is the same Skippr SQL as `SELECT` on that table. It tails new results to stdout as they arrive, like `watch`. `LIMIT` stops the stream. `--watch` is a poll of `SELECT`, not `STREAM`.

```bash
skipprd query --sql "STREAM * FROM bikehire.rides LIMIT 100"
```

`SELECT` is a snapshot of Iceberg plus the WAL slices Skippr Store pinned for that statement. `STREAM` follows new live rows on that same table. A second query process can run while `sync` holds the writer. It pins a flushed Skippr Store snapshot and does not take `writer.lock`. See [Stream SQL](/concepts/stream-sql) and [Skippr Store](/configuration/skippr-store).

`skipprd serve` Flight SQL reads committed Iceberg only. Live WAL stays on `skipprd query`.

### Inspect a pipeline

```bash
skipprd query --sql "SHOW PIPELINE bikehire"
skipprd query --sql "SHOW STATS FOR bikehire.rides"
```

### Pause, resume, and reset a pipeline

```bash
skipprd query --sql "DISABLE PIPELINE bikehire"
skipprd query --sql "ENABLE PIPELINE bikehire"
skipprd query --sql "RESET PIPELINE bikehire"
skipprd query --sql "DROP PIPELINE bikehire"
```

`RESET PIPELINE` clears the pipeline's offsets and buffered WAL data immediately, so the next sync reads the source from the start. `DROP PIPELINE` removes all schemas and data for the pipeline immediately.

### Manage schemas

```bash
skipprd query --sql "SCHEMA DUMP bikehire TO 'bikehire-schema.json'"
skipprd query --sql "LOAD SCHEMA 'bikehire-schema.json' INTO bikehire"
skipprd query --sql "ALTER TABLE bikehire.rides DROP COLUMN legacy_field"
skipprd query --sql "ALTER TABLE bikehire.rides ALTER COLUMN n TYPE BIGINT"
skipprd query --sql "ALTER TABLE bikehire.rides RENAME COLUMN region TO detail.region"
skipprd query --sql "ALTER TABLE bikehire.rides MERGE COLUMN price_string INTO price"
```

Flattened Iceberg names (`detail_truck_reg`) and skippr dotted paths (`detail.truck_reg`) both resolve. Iceberg sequential field ids are matched by name when they differ from skippr's hashed ids.

`ALTER TABLE` applies in-stream; `DISABLE PIPELINE` is freeze-only. Prefer [`skipprd migrate`](/cli/migrate) for file-backed batches. See [ALTER operations](/operations/alter) for rename, merge, drop, type promote, and what fails closed.

Run [`skipprd sql-help`](/cli/sql-help) for the syntax of every statement, or `skipprd query --sql "SHOW DOCS"`.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Error: All tables must be referenced as <pipeline>.<namespace>` | The interactive view needs a fully qualified table name. The message lists the allowed tables. | Use `<pipeline>.<namespace>`, for example `bikehire.rides`. |
| `ERROR: Unqualified table name detected` | A table in the query has no pipeline prefix. | Qualify every table. |
| `query failed: ... table ... not found` with `--plain` | The table name is wrong, or the pipeline has not been discovered or synced yet. | Run `skipprd metadata show --pipeline <name>` to list namespaces. |
| `Pipeline '<name>' not found` | The pipeline in a management statement has no saved schema yet. | Run `skipprd discover --pipeline <name>` first. |
| Fewer rows than in your warehouse | Non-Iceberg destinations only expose data still in the WAL. | Query the warehouse directly for full history. |

In scripts, use `--plain`: a failing `SELECT` then prints `query failed: <reason>` to stderr and exits `1`. Without `--plain`, some errors are printed but the command still exits `0`.

## Next steps

- [Stream SQL](/concepts/stream-sql)
- [skipprd sql-help](/cli/sql-help)
- [skipprd serve](/cli/serve)
- [skipprd df](/cli/df)
- [skipprd migrate](/cli/migrate)
- [Streamhouse](/concepts/datalake)
- [Deadletters](/concepts/deadletters)
