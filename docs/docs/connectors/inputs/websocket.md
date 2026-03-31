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
