# Skipprd schema

Display the discovered schema for a pipeline.

## Usage

```bash
skipprd schema --pipeline <name> [--log [LEVEL]]
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--pipeline, -p` | Yes | Pipeline name. |
| `--log` | No | Enable logging. |

## What it does

Loads the pipeline metadata from S3 and prints the discovered schema: field names, types, nesting structure, and namespace definitions.
