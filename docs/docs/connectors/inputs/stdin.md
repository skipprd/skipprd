---
title: Stdin
description: Read records from standard input. Useful for pipes and one-off loads.
---

# Stdin

Reads JSON or another `format` from stdin. Use it to pipe a command into Skipprd. This is not a long-running server — the process ends when stdin closes.

## Before you begin

- A producer that writes records to stdout (one JSON object per line for `json`).

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceStdin, Pipeline

cfg = Config.discover()
src = cfg.data_source("pipe", DataSourceStdin(mode="line", format="json"))
cfg.pipeline("pipe", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source stdin \
  --pipeline pipe \
  --name pipe \
  --mode line \
  --format json
```

```yaml [YAML]
data_sources:
  pipe:
    Stdin:
      mode: line
      format: json
```

:::

```bash
cat events.jsonl | skipprd sync --pipeline pipe --once
```

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `mode` | string | Not set | How stdin is framed (for example `line`) |
| `format` | string | Not set | Payload format (`json`, …) |
| `batch_size_bytes` | integer | Not set | Max bytes per batch |
| `batch_size_seconds` | integer | Not set | Max seconds per batch |

## What gets synced

Each framed payload becomes a record. There is no resume position beyond what the WAL already committed. If you re-run the same pipe, rows can duplicate in append-only destinations.

## Troubleshooting

| Symptom | Fix |
|---|---|
| No rows | Confirm the producer writes to stdout and the pipe is attached |
| Parse errors | Match `format` to the payload; JSON must be one object per line in `line` mode |

## Next steps

- [HTTP server](/connectors/inputs/http_server) — for push over the network
- [Local file source](/connectors/inputs/file)
