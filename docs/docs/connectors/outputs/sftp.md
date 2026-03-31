# SFTP Output

Uploads Parquet files to a remote SFTP server.

## How it works

1. Serializes record batches to Parquet.
2. Connects via SSH and uploads to the configured remote path.

## Configuration

```yaml
data_sinks:
  sink:
    Sftp:
      host: sftp.example.com
      port: 22
      username: user
      password: secret
      remote_path: "/data/output"
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `host` | *(required)* | SFTP server hostname |
| `port` | `22` | SSH port |
| `username` | *(required)* | SSH username |
| `password` | | Password authentication |
| `private_key_path` | | Path to SSH private key |
| `remote_path` | *(required)* | Remote directory for uploads |
| `format` | `parquet` | Output format |
