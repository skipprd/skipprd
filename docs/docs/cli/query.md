---
title: skipprd query
description: Run SQL against your pipelines from the terminal or a script, and manage pipelines and schemas with Skipprd SQL statements.
---

# skipprd query

`skipprd query` runs SQL against the data your pipelines have ingested. Each namespace is a table named `<pipeline>.<namespace>`, for example `bikehire.rides`. What a table contains depends on the destination:

- **No destination, or a non-Iceberg destination** (Athena, Snowflake, Postgres, and others): the live data in the WAL. For the full history in a warehouse, query the warehouse itself.
- **Iceberg destination** (SkipprLake, AthenaIceberg, DuckDB): the Iceberg table combined with live data still in the WAL, so you see rows the moment they are ingested.

Use `query` to check a sync, explore data, script checks in CI, and run Skipprd's management statements (`ENABLE PIPELINE`, `SCHEMA DUMP`, and others). Without `--sql` it opens an interactive prompt.

## Usage

::: code-group

```python [Python]
import skippr

session = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
table = session.query("SELECT count(*) AS rides FROM bikehire.rides")
print(table.to_pylist())
```

```bash [CLI]
skipprd query --sql "<SQL>" [--plain] [--watch <seconds>] [--config <path>] [--log [LEVEL]]
skipprd query                      # interactive prompt
```

:::

`query` reads every pipeline in `skippr.yml`; there is no `--pipeline` flag. Name tables in full.

## Options

| Flag | Default | Description |
|---|---|---|
| `-s`, `--sql <SQL>` | None | Statement to run. Without it, `query` opens an interactive `sql>` prompt; type `:q`, `:quit`, or `exit` to leave. |
| `--plain` | Off | Print one JSON document to stdout instead of the interactive table view: `{"header": [...], "rows": [[...]]}`. All values are strings. Use this in scripts. |
| `--watch <SECONDS>` | Off | Re-run a `SELECT` every N seconds in the interactive table view. Ignored with `--plain`. |
| `--config <PATH>` | `./skippr.yml` | Config file to read. |
| `--log [LEVEL]` | Off | Print logs to stderr. |
| `--wal-storage <MODE>` | `disk` | With `clustered`, `--sql` is required and `SELECT` results are always printed as JSON. |

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

### Stream new records from the WAL

```bash
skipprd query --sql "STREAM * FROM bikehire LIMIT 100"
```

`STREAM` reads data as it is ingested and keeps returning new results.

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

`RESET PIPELINE` clears the pipeline's offsets and buffered WAL data, so the next sync reads the source from the start. `DROP PIPELINE` removes all schemas and data for the pipeline. When run from `query`, both take effect on the pipeline's next sync.

### Manage schemas

```bash
skipprd query --sql "SCHEMA DUMP bikehire TO 'bikehire-schema.json'"
skipprd query --sql "LOAD SCHEMA 'bikehire-schema.json' INTO bikehire"
skipprd query --sql "DISABLE PIPELINE bikehire"
skipprd query --sql "ALTER TABLE bikehire.rides DROP COLUMN legacy_field"
skipprd query --sql "ALTER TABLE bikehire.rides ALTER COLUMN n TYPE BIGINT"
skipprd query --sql "ALTER TABLE bikehire.rides RENAME COLUMN region TO detail.region"
skipprd query --sql "ALTER TABLE bikehire.rides MERGE COLUMN price_string INTO price"
skipprd query --sql "ENABLE PIPELINE bikehire"
```

Flattened Iceberg names (`detail_truck_reg`) and skippr dotted paths (`detail.truck_reg`) both resolve. Iceberg sequential field ids are matched by name when they differ from skippr's hashed ids.

Run [`skipprd sql-help`](/cli/sql-help) for the syntax of every statement, or `skipprd query --sql "SHOW DOCS"`.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Error: All tables must be referenced as <pipeline>.<namespace>` | The interactive view needs a fully qualified table name. The message lists the allowed tables. | Use `<pipeline>.<namespace>`, for example `bikehire.rides`. |
| `ERROR: Unqualified table name detected` | A table in the query has no pipeline prefix. | Qualify every table. |
| `query failed: ... table ... not found` with `--plain` | The table name is wrong, or the pipeline has not been discovered or synced yet. | Run `skipprd metadata show --pipeline <name>` to list namespaces. |
| `Pipeline '<name>' not found` | The pipeline in a management statement has no saved schema yet. | Run `skipprd discover --pipeline <name>` first. |
| Fewer rows than in your warehouse | Non-Iceberg destinations only expose data still in the WAL. | Query the warehouse directly for full history. |
| `clustered query requires --sql` | The interactive prompt is not available in clustered mode. | Pass `--sql`. |

In scripts, use `--plain`: a failing `SELECT` then prints `query failed: <reason>` to stderr and exits `1`. Without `--plain`, some errors are printed but the command still exits `0`.

## Next steps

- [skipprd sql-help](/cli/sql-help)
- [skipprd df](/cli/df)
- [Datalake](/concepts/datalake)
- [Deadletters](/concepts/deadletters)
