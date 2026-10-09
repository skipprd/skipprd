---
title: SFTP source
description: Pull JSON, CSV, or Parquet from a remote directory over SFTP.
---

# SFTP

Reads files from `remote_path` on an SFTP server. Use it when a partner still drops files that way.

## Before you begin

- Host, username, and a readable remote directory.
- A password **or** a private key.

```bash
export SFTP_PASSWORD="change-me"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceSftp, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "partner",
    DataSourceSftp(
        host="sftp.partner.example",
        username="skippr",
        remote_path="/export/daily",
        port=22,
        password=EnvRef("SFTP_PASSWORD"),
        format="csv",
    ),
)
cfg.pipeline("partner", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source sftp \
  --pipeline partner \
  --name partner \
  --host sftp.partner.example \
  --username skippr \
  --remote-path /export/daily \
  --port 22 \
  --password '${SFTP_PASSWORD}' \
  --format csv
```

```yaml [YAML]
data_sources:
  partner:
    Sftp:
      host: sftp.partner.example
      username: skippr
      remote_path: /export/daily
      port: 22
      password: ${SFTP_PASSWORD}
      format: csv
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `host` | string | Required | SFTP host |
| `username` | string | Required | User |
| `remote_path` | path | Required | Remote directory or file |
| `port` | integer | Not set | Port (default 22) |
| `password` | secret | Not set | Password as `${ENV}` |
| `private_key_path` | path | Not set | Private key file |
| `format` | string | Inferred | `json`, `csv`, or `parquet` |
| `batch_size_bytes` | integer | Not set | Max bytes per batch |
| `batch_size_seconds` | integer | Not set | Max seconds per batch |

## What gets synced

Each file becomes records. Progress is per remote file. A file that changes after a commit is read again.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Connection refused | Check `host`, `port`, and firewall |
| Auth failed | Confirm password or key |
| No files | Check `remote_path` and the user's directory listing rights |

## Next steps

- [S3 source](/connectors/inputs/s3)
- [SFTP destination](/connectors/outputs/sftp)
