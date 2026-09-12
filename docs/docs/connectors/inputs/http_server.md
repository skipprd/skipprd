# HTTP Server Input

Listens for incoming HTTP POST requests and ingests their bodies.

## How it works

1. Starts an HTTP server on the configured address and path.
2. Accepts POST requests; each request body is ingested as a record.
3. Optional Bearer token authentication via the `auth_token` field.
4. Namespace convention: `http_server.{path}`.

## Configuration

```yaml
data_sources:
  source:
    HttpServer:
      listen_address: "0.0.0.0:8080"
      path: "/"
      auth_token: "secret"
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `listen_address` | `0.0.0.0:8080` | Address to bind the HTTP server |
| `path` | `/` | URL path to listen on |
| `auth_token` | | Optional Bearer token for authentication |
| `format` | `json` | Data format |

## Authentication

Authentication is optional. If you set `auth_token`, Skipprd expects the incoming request to present that bearer token.

## Troubleshooting

| Symptom | Fix |
|---|---|
| requests receive 401 or 403 responses | Verify the bearer token handling and any proxy configuration in front of the listener. |
| no requests arrive | Check the bind address, port, reverse proxy routing, and host firewall rules. |
