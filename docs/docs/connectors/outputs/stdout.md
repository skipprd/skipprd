# Stdout Output

Prints each record batch as line-delimited JSON to standard output.

## How it works

1. Receives compacted or batched records from the sink pipeline.
2. Serializes batches as JSON lines (one JSON object per line) to `stdout`.
3. No network or filesystem destination is required.

## Configuration

```bash
DATA_OUTPUT_PLUGIN_NAME=Stdout
```

Or via YAML pipeline config:

```yaml
data_sinks:
  destination:
    Stdout: {}
```

No additional fields are required. This sink is useful for debugging pipelines and for piping Skippr output into other tools.

## Configuration variables

There are no sink-specific environment variables. Use `DATA_OUTPUT_PLUGIN_NAME=Stdout` (or the equivalent YAML `Stdout: {}` entry) only.
