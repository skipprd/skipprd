---
title: skipprd df
description: Print every row of a pipeline or one of its namespaces as JSON, the CLI twin of Python Session.df().
---

# skipprd df

`skipprd df` prints every row of one namespace, or of all namespaces in a pipeline, as a single JSON document. It is `SELECT *` without writing SQL, and the CLI twin of Python `Session.df()`. Use it for a quick look at small datasets, in tests, or to hand data to another tool. It reads the same tables as [`skipprd query`](/cli/query): live WAL data, combined with the Iceberg table when the destination is SkipprLake, AthenaIceberg, or DuckDB. For large tables, use `skipprd query --plain` with a `LIMIT` instead.

## Usage

::: code-group

```python [Python]
import skippr

session = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
everything = session.df()           # all namespaces in the pipeline
rides = session.df("rides")         # one namespace
print(rides.num_rows)
```

```bash [CLI]
skipprd df --pipeline <name> [--namespace <namespace>]
skipprd df --namespace <pipeline>.<namespace>
```

:::

## Options

| Flag | Default | Description |
|---|---|---|
| `-p`, `--pipeline <NAME>` | `PIPELINE_NAME` env var | Pipeline to read. Required unless `--namespace` is written as `<pipeline>.<namespace>`. |
| `--namespace <NAME>` | All namespaces | One namespace, as `rides` (uses `--pipeline`) or `bikehire.rides`. Leave it out to read every namespace in the pipeline. |
| `--config <PATH>` | `./skippr.yml` | Config file to read. |
| `--log [LEVEL]` | Off | Print logs to stderr. Errors are only shown on screen with this flag. |

See [CLI overview](/cli/overview) for the other global flags.

The output has the same shape as `skipprd query --plain`: `{"header": [...], "rows": [[...]]}`, with every value as a string.

## Examples

### One namespace

```bash
skipprd df --pipeline bikehire --namespace rides
```

```json
{"header":["ride_id","station","duration_s","member"],"rows":[["1","Waterloo","420","true"],["2","Euston","615","false"]]}
```

### Fully qualified name

```bash
skipprd df --namespace bikehire.rides
```

### Every namespace in a pipeline

```bash
skipprd df --pipeline bikehire
```

Rows from all namespaces are combined into one document. This only works when every namespace has the same columns; if they differ, the command fails, so read them one at a time with `--namespace`.

### Pipe into another tool

```bash
skipprd df --namespace bikehire.rides | jq '.rows | length'
```

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Exits `1` with no output | The error is only printed with `--log`. | Rerun with `--log`. |
| `df() requires a pipeline when listing all namespaces` | No `--pipeline`, no `PIPELINE_NAME`, and no `--namespace`. | Pass `--pipeline`. |
| `df("namespace") requires Session.pipeline` | `--namespace rides` was given without a pipeline. | Pass `--pipeline`, or write `--namespace bikehire.rides`. |
| `df name must be namespace or pipeline.namespace` | The name has more than one `.`. | Use `rides` or `bikehire.rides`. |
| `{"header":[],"rows":[]}` | The pipeline has no namespaces or no data yet. | Run `skipprd discover` and `skipprd sync` first. |

## Next steps

- [skipprd query](/cli/query)
- [Python](/python)
