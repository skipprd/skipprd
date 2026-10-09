---
title: SFTP destination
description: Upload Skipprd batches to a remote directory over SFTP.
---

# SFTP

Uploads each batch as a file on an SFTP server. Use it when a partner still collects files that way.

## Before you begin

- Host, username, and a remote directory the user can write.
- A password **or** a private key path.

```bash
export SFTP_PASSWORD="change-me"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkSftp, EnvRef, Pipeline

cfg = Config.discover()
out = cfg.data_sink(
    "partner",
    DataSinkSftp(
        host="sftp.partner.example",
        username="skippr",
        remote_path="/incoming/skipprd",
        port=22,
        password=EnvRef("SFTP_PASSWORD"),
    ),
)
cfg.pipeline("export", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=out))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink sftp \
  --pipeline export \
  --name partner \
  --host sftp.partner.example \
  --username skippr \
  --remote-path /incoming/skipprd \
  --port 22 \
  --password '${SFTP_PASSWORD}'
```

```yaml [YAML]
data_sinks:
  partner:
    Sftp:
      host: sftp.partner.example
      username: skippr
      remote_path: /incoming/skipprd
      port: 22
      password: ${SFTP_PASSWORD}
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `host` | string | Required | SFTP host |
| `username` | string | Required | User |
| `remote_path` | path | Required | Remote directory |
| `port` | integer | Not set | Port (default 22) |
| `password` | secret | Not set | Password as `${ENV}` |
| `private_key_path` | path | Not set | Private key file (instead of a password) |
| `format` | string | Not set | File format override |

A retry overwrites the same remote path.

## How data lands

Files are uploaded under `remote_path/<namespace>/`.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Connection refused | Check `host`, `port`, and firewall |
| Auth failed | Confirm password or key, and that the key has no passphrase Skipprd cannot supply |
| Permission denied | Grant write on `remote_path` |

## Next steps

- [SFTP source](/connectors/inputs/sftp)
- [S3 destination](/connectors/outputs/s3)
