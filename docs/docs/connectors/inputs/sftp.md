# SFTP Input

Downloads files from an SFTP server and ingests their contents.

## How it works

1. Connects to the SFTP server via SSH.
2. Lists files matching `remote_path` (supports glob patterns).
3. Downloads each file and ingests its contents.
4. Namespace convention: `sftp.{filename}`.

## Configuration

```yaml
data_sources:
  source:
    Sftp:
      host: sftp.example.com
      port: 22
      username: user
      password: secret
      remote_path: "/data/*.json"
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `host` | *(required)* | SFTP server hostname |
| `port` | `22` | SSH port |
| `username` | *(required)* | SSH username |
| `password` | | Password authentication |
| `private_key_path` | | Path to SSH private key |
| `remote_path` | *(required)* | Remote file path or glob |
| `format` | `json` | Data format |
