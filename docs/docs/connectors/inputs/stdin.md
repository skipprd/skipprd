# Stdin Input

Reads line-delimited data from standard input.

## Supported formats

- Depends on the optional `format` setting (e.g. JSON lines). Each line is typically one record.

## How it works

1. Reads from `stdin` until EOF (**batch mode**, default) or continuously (**stream mode**).
2. **Batch mode:** suitable for pipes and one-shot jobs; ingestion completes when the stream closes.
3. **Stream mode:** keeps reading as new lines arrive for long-running processes.
4. Namespace convention: `stdin`.

## Configuration

```bash
DATA_SOURCE_PLUGIN_NAME=Stdin
```

Or via YAML pipeline config:

```yaml
data_sources:
  source:
    Stdin:
      mode: batch
      format: jsonl
```

## Typical usage

Pipe data into Skippr:

```bash
cat data.json | skipprd sync --pipeline my_pipeline
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `mode` | `batch` | `batch` (read until EOF) or `stream` (continuous) |
| `format` | | Optional format hint for the parser |

## Namespace convention

```
stdin
```
