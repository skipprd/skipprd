# Socket Input

Listens on a TCP, UDP, or Unix socket for incoming data.

## How it works

1. Binds to the configured address using the specified socket mode.
2. **TCP:** accepts connections and reads newline-delimited data.
3. **UDP:** receives datagrams and processes each line.
4. **Unix:** accepts connections on a Unix domain socket (unix only).
5. Namespace convention: `socket.{mode}.{address}`.

## Configuration

```yaml
data_sources:
  source:
    Socket:
      mode: tcp
      address: "0.0.0.0:9000"
      framing: newline
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `mode` | *(required)* | `tcp`, `udp`, or `unix` |
| `address` | *(required)* | Bind address (host:port or socket path) |
| `framing` | `newline` | Frame delimiter (`newline` or `bytes`) |
| `format` | `json` | Data format |

## Authentication

No connector-specific authentication is required.

## Troubleshooting

| Symptom | Fix |
|---|---|
| address already in use | Choose a different port or stop the process currently bound to that address. |
| no data arriving | Check the sender target address, protocol mode, and any host firewall rules. |
