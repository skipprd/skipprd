# WebSocket Input

Connects to a WebSocket server and ingests received messages.

## How it works

1. Establishes a WebSocket connection to the configured URL.
2. Receives text and binary messages, converting binary to UTF-8.
3. Supports stream and batch modes.
4. Namespace convention: `websocket.{url_host}`.

## Configuration

```yaml
data_sources:
  source:
    Websocket:
      url: "ws://localhost:8080/stream"
      mode: batch
      idle_timeout_seconds: 10
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `url` | *(required)* | WebSocket URL (ws:// or wss://) |
| `headers` | | Additional request headers |
| `ping_interval_seconds` | `30` | Ping interval |
| `mode` | `stream` | `stream` or `batch` |
| `idle_timeout_seconds` | `5` | Batch mode idle timeout |
| `format` | `json` | Data format |

## Authentication

Authentication is optional and depends on the upstream service. For security best practices, we strongly advise against storing authentication header values in `skippr.yml`. Use environment variable interpolation instead: replace the relevant `headers` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sources:
  source:
    Websocket:
      headers:
        Authorization: "${WEBSOCKET_AUTH_HEADER}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export WEBSOCKET_AUTH_HEADER="Bearer your-token"
```

Windows PowerShell

```powershell
$env:WEBSOCKET_AUTH_HEADER = "Bearer your-token"
```

Windows Command Prompt

```cmd
set WEBSOCKET_AUTH_HEADER=Bearer your-token
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| handshake failed | Verify the URL, any required headers, and whether the endpoint expects `ws://` or `wss://`. |
| connection drops repeatedly | Check idle timeouts, keepalive settings, and proxy or firewall behavior between the runner and server. |
