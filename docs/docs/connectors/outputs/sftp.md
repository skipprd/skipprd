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

## Authentication

Use either a password or an SSH private key. Prefer private-key auth for long-lived pipelines. For security best practices, we strongly advise against storing the password in `skippr.yml`. Use environment variable interpolation instead: replace the `password` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sinks:
  warehouse:
    Sftp:
      password: "${SFTP_PASSWORD}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export SFTP_PASSWORD="secret"
```

Windows PowerShell

```powershell
$env:SFTP_PASSWORD = "secret"
```

Windows Command Prompt

```cmd
set SFTP_PASSWORD=secret
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| authentication failed | Verify the username, password or private key path, and any host-based access controls. |
| uploads fail | Check `remote_path`, available disk space, and whether the SSH user can create files in that directory. |
