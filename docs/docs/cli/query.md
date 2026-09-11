# query

Run engine SQL against configured data, manage pipelines and schemas, and
stream from the WAL.

Use `skipprd query` for the local runtime. `sde query` is a different
command: warehouse SQL through the modeling stack, not engine pipeline SQL.

## Usage

```bash
skipprd --config skippr.yml query --sql "<SQL>" [--watch <seconds>] [--plain] [--log [LEVEL]]
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--sql, -s` | No | The SQL statement to execute. If omitted, opens interactive mode. |
| `--watch` | No | Re-run the query every N seconds (live refresh). |
| `--plain` | No | Print plain-text results to stdout instead of the TUI. |
| `--log` | No | Enable logging. |

## Examples

Query a table:

```bash
skipprd query --sql "SELECT COUNT(*) FROM bikehire"
```

Live watch:

```bash
skipprd query --sql "SELECT COUNT(*) FROM bikehire" --watch 5
```

Pipeline management:

```bash
skipprd query --sql "ENABLE PIPELINE bikehire"
skipprd query --sql "DISABLE PIPELINE bikehire"
skipprd query --sql "RESET PIPELINE bikehire"
skipprd query --sql "DROP PIPELINE bikehire"
```

Schema management:

```bash
skipprd query --sql "SCHEMA DUMP bikehire TO 'schema.json'"
skipprd query --sql "LOAD SCHEMA 'schema.json' INTO bikehire"
skipprd query --sql "ALTER SCHEMA bikehire DROP COLUMN old_field"
skipprd query --sql "ALTER SCHEMA bikehire ALTER COLUMN price TYPE DECIMAL(10,2)"
```

Stream from the WAL:

```bash
skipprd --config skippr.yml query --sql "STREAM * FROM bikehire LIMIT 100"
```

See the [SQL Reference](../sql/reference.md) for all supported statements.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Pipeline not found (for `ENABLE PIPELINE`), or query error |
