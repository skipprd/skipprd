---
title: Local file source
description: Read JSON, CSV, or Parquet from a path on disk.
---

# Local file

Reads files from `path`. Use it for a laptop sample, a nightly drop directory, or CI fixtures.

## Before you begin

- A file or directory Skipprd can read.
- Format is inferred from the extension when you omit `format`.

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceFile, Pipeline

cfg = Config.discover()
src = cfg.data_source("sample", DataSourceFile(path="./data/events.jsonl", format="json"))
cfg.pipeline("sample", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source file \
  --pipeline sample \
  --name sample \
  --path ./data/events.jsonl \
  --format json
```

```yaml [YAML]
data_sources:
  sample:
    File:
      path: ./data/events.jsonl
      format: json
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `path` | path | Required | File or directory |
| `format` | string | Inferred | `json`, `csv`, or `parquet` |
| `batch_size_seconds` | integer | Not set | Max seconds per batch |
| `batch_size_bytes` | integer | Not set | Max bytes per batch |

## What gets synced

Each file becomes records. If `path` is a directory, Skipprd reads the files in it. Progress is per file; a file that changes after a commit is read again.

## Troubleshooting

| Symptom | Fix |
|---|---|
| File not found | Use a path relative to the working directory, or an absolute path |
| Parse errors | Set `format`, and check the file is UTF-8 |
| Permission denied | Grant read on `path` |

## Next steps

- [S3 source](/connectors/inputs/s3)
- [Local file destination](/connectors/outputs/file)
