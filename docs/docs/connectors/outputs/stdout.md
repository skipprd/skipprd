---
title: Stdout
description: Print Skipprd batches to standard output. Debugging only — retries are not applied.
---

# Stdout

Writes each batch to stdout. Use it to see records while you wire a source. Do not use it as a production destination: a crash is not retried, and nothing is durable. See [Exactly-once delivery](/concepts/exactly-once).

## Before you begin

- A terminal or log collector that can take the process stdout.

## Configure

This destination has no options.

::: code-group

```python [Python]
from skippr import Config, DataSinkStdout, Pipeline

cfg = Config.discover()
out = cfg.data_sink("out", DataSinkStdout())
cfg.pipeline("sample", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=out))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink stdout \
  --pipeline sample \
  --name out
```

```yaml [YAML]
data_sinks:
  out:
    Stdout: {}
```

:::

## Options

This connector has no configuration keys.

## How data lands

Records are printed as they flush. Redirect stdout if you want a file (`skipprd sync --once > batch.jsonl`). Prefer the [local file](/connectors/outputs/file) destination when you need paths you can keep.

## Troubleshooting

| Symptom | Fix |
|---|---|
| No output | Confirm the source produced rows (`--log`) and you did not redirect stdout away |
| Mixed with logs | Logs go to stderr; keep `--log` on and read stdout separately |

## Next steps

- [Local file destination](/connectors/outputs/file)
- [Logging](/operations/logging)
