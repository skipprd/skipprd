# skippr-el query

Run SQL against destination tables, manage pipelines and schemas.

## Usage

```bash
skippr-el query --sql "<SQL>" [--watch <seconds>] [--plain] [--log [LEVEL]]
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
skippr-el query --sql "SELECT COUNT(*) FROM bikehire"
```

Live watch:

```bash
skippr-el query --sql "SELECT COUNT(*) FROM bikehire" --watch 5
```

Pipeline management:

```bash
skippr-el query --sql "ENABLE PIPELINE bikehire"
skippr-el query --sql "DISABLE PIPELINE bikehire"
skippr-el query --sql "RESET PIPELINE bikehire"
skippr-el query --sql "DROP PIPELINE bikehire"
```

Schema management:

```bash
skippr-el query --sql "SCHEMA DUMP bikehire TO 'schema.json'"
skippr-el query --sql "LOAD SCHEMA 'schema.json' INTO bikehire"
skippr-el query --sql "ALTER SCHEMA bikehire DROP COLUMN old_field"
skippr-el query --sql "ALTER SCHEMA bikehire ALTER COLUMN price TYPE DECIMAL(10,2)"
```

Stream from the WAL:

```bash
skippr-el query --sql "STREAM * FROM bikehire LIMIT 100"
```

See the [SQL Reference](../sql/reference.md) for all supported statements.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Pipeline not found (for `ENABLE PIPELINE`), or query error |
