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
| `format` | string | Not set | For pipeline type `ingest`, set `format` on this data sink to `parquet` or `jsonl`. For pipeline type `copy`, set `format` on this data sink to `jsonl`, `csv`, or `xml`. If you omit `format` on a copy pipeline, the data sink follows the source: json → jsonl, csv → csv, xml → xml. In pipeline type `copy`, nested objects and arrays in CSV are written as compact JSON strings. When pipeline type is `copy`, the deadletter sink still writes `parquet` or `jsonl`; it does not follow a `csv` or `xml` data sink. |

A retry overwrites the same path.

## How data lands

Files land at `output_dir/<namespace>/…`.

- `<namespace>` is the source namespace (table or stream name).
- Optional partition folders follow the namespace: `p_country=GB/` from `batch_partition_fields`, and `year=2026/month=10/day=9/` from `batch_time_fields` (or `p_year=…` when `time_partition_prefix` is `p_`).
- The file name is the batch identity plus the `format` extension, for example `apply-0001.jsonl`.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Permission denied | Choose a writable `output_dir` |
| Disk full | Free space, or point `DATA_DIR` and `output_dir` at a larger volume |

## Next steps

- [DuckDB](/connectors/outputs/duckdb)
- [Stdout](/connectors/outputs/stdout)
