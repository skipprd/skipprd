---
title: Local file destination
description: Write Skipprd batches as files on disk. Use it for debugging or a local hand-off.
---

# Local file

Writes each batch into `output_dir`. Nothing is uploaded. Use this to inspect what a pipeline produces, or to feed another local process.

## Before you begin

- A directory Skipprd can create and write.

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkFile, Pipeline

cfg = Config.discover()
out = cfg.data_sink("out", DataSinkFile(output_dir="./out", format="parquet"))
cfg.pipeline("sample", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=out))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink file \
  --pipeline sample \
  --name out \
  --output-dir ./out \
  --format parquet
```

```yaml [YAML]
data_sinks:
  out:
    File:
      output_dir: ./out
      format: parquet
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `output_dir` | path | Required | Directory for files |
| `format` | string | Not set | File format (`parquet`, `json`, …) |

A retry overwrites the same path.

## How data lands

Files appear under `output_dir/<namespace>/`. This destination is not a warehouse. For local SQL, use [DuckDB](/connectors/outputs/duckdb).

## Troubleshooting

| Symptom | Fix |
|---|---|
| Permission denied | Choose a writable `output_dir` |
| Disk full | Free space, or point `DATA_DIR` and `output_dir` at a larger volume |

## Next steps

- [DuckDB](/connectors/outputs/duckdb)
- [Stdout](/connectors/outputs/stdout)
