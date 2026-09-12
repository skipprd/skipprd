# HTTP Client Input

Fetches data from an HTTP endpoint. Supports one-shot or periodic polling.

## How it works

1. Sends an HTTP request (GET/POST/PUT) to the configured URL.
2. Without `scrape_interval_seconds`: performs a one-shot fetch and completes.
3. With `scrape_interval_seconds`: polls at the given interval until shutdown.
4. Supports Basic and Bearer auth, custom headers, request body, and gzip decompression.
5. Namespace convention: `http.{url_host}`.

## Configuration

```yaml
data_sources:
  source:
    HttpClient:
      url: "https://api.example.com/data"
      method: GET
      scrape_interval_seconds: 60
      auth:
        strategy: bearer
        token: "my-token"
      headers:
        Accept: application/json
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `url` | *(required)* | HTTP endpoint URL |
| `method` | `GET` | HTTP method (GET, POST, PUT) |
| `headers` | | Map of additional request headers |
| `body` | | Request body string |
| `auth.strategy` | | `basic` or `bearer` |
| `auth.user` / `auth.password` | | Credentials for basic auth |
| `auth.token` | | Token for bearer auth |
| `scrape_interval_seconds` | | Polling interval; omit for one-shot |
| `scrape_timeout_seconds` | `5` | Request timeout |
| `format` | `json` | Data format |

## Authentication

Authentication is optional and depends on the upstream endpoint. For security best practices, we strongly advise against storing authentication values in `skippr.yml`. Use environment variable interpolation instead: replace the relevant auth field with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sources:
  source:
    HttpClient:
      auth_strategy: bearer
      auth_token: "${HTTP_CLIENT_AUTH_TOKEN}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export HTTP_CLIENT_AUTH_TOKEN="your-token"
```

Windows PowerShell

```powershell
$env:HTTP_CLIENT_AUTH_TOKEN = "your-token"
```

Windows Command Prompt

```cmd
set HTTP_CLIENT_AUTH_TOKEN=your-token
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| 401 or 403 responses | Verify credentials, bearer tokens, and any required request headers. |
| timeouts or empty responses | Check the endpoint URL, method, timeout settings, and whether the service is rate limiting requests. |
