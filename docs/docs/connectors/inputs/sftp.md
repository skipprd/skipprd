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

## Authentication

Use either a password or an SSH private key. Prefer private-key auth for long-lived pipelines. For security best practices, we strongly advise against storing the password in `skippr.yml`. Use environment variable interpolation instead: replace the `password` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sources:
  source:
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
| no files found | Check `remote_path` and confirm the SSH user can list and read that location. |
