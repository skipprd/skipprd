---
title: HTTP client source
description: Fetch JSON, CSV, or XML from an HTTP endpoint with Skipprd, once per run or on a polling interval, and land the response as records.
---

# HTTP client source

The HTTP client source sends a request to an HTTP or HTTPS endpoint and lands the response body as records. Use it to load a JSON export, a CSV report URL, or a small API that returns everything in one response. Without `scrape_interval_seconds` it fetches once per run; with it, Skipprd polls on that interval.

This source doesn't paginate or track what it has already fetched. For APIs with pages or cursors, use a dedicated connector from the [catalog](/connectors/).

## Before you begin

You need:

- An `http://` or `https://` URL that the machine running Skipprd can reach.
- Credentials if the endpoint needs them: a bearer token or a Basic auth username and password.
- A response format Skipprd can parse: JSON (a single object, an array, or newline-delimited JSON), CSV, or XML. A URL containing `.gz` is decompressed as gzip.

## Configure

Keep the token in an environment variable. `skippr.yml` stores only the reference.

```bash
export STATUS_API_TOKEN="your-token"
```

::: code-group

```python [Python]
from skippr import (
    Config,
    DataSourceHttpClient,
    DataSourceHttpClientDataSourceHttpAuthConfig,
    EnvRef,
    Pipeline,
)

cfg = Config.discover()
src = cfg.data_source(
    "status_api",
    DataSourceHttpClient(
        url="https://status.example.com/api/incidents",
        headers={"Accept": "application/json"},
        auth=DataSourceHttpClientDataSourceHttpAuthConfig(
            strategy="bearer",
            token=EnvRef("STATUS_API_TOKEN"),
        ),
        scrape_interval_seconds=300,
    ),
)
cfg.pipeline("incidents", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source http-client \
  --pipeline incidents \
  --name status_api \
  --url https://status.example.com/api/incidents \
  --headers '{Accept: application/json}' \
  --auth-strategy bearer \
  --auth-token '${STATUS_API_TOKEN}' \
  --scrape-interval-seconds 300
```

```yaml [YAML]
pipelines:
  incidents:
    data_source: data_sources.status_api

data_sources:
  status_api:
    HttpClient:
      url: "https://status.example.com/api/incidents"
      headers:
        Accept: application/json
      auth:
        strategy: bearer
        token: "${STATUS_API_TOKEN}"
      scrape_interval_seconds: 300
```

:::

Run the pipeline and check the result:

1. Fetch once to check the response parses. For a one-off fetch, leave out `scrape_interval_seconds`.

   ```bash
   skipprd sync --pipeline incidents --once --log
   ```

2. Read what landed:

   ```bash
   skipprd df --pipeline incidents
   ```

To load the data into a warehouse, add a destination to the pipeline. See [Destinations](/configuration/output).

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `url` | string | Required | Endpoint URL. |
| `method` | string | `GET` | `GET`, `POST`, or `PUT`. Any other value sends `GET`. |
| `headers` | map of strings | — | Extra request headers. Each value must be a literal or a whole `${NAME}` reference. |
| `body` | string | — | Request body, sent as is. Set a matching `Content-Type` in `headers`. |
| `auth.strategy` | string | — | `bearer` sends `auth.token`. `basic` sends `auth.user` and `auth.password`. Other values send no credentials. |
| `auth.user` | string | — | Basic auth username. |
| `auth.password` | secret | — | Basic auth password as a `${NAME}` reference. |
| `auth.token` | secret | — | Bearer token as a `${NAME}` reference. |
| `scrape_interval_seconds` | integer | — | Seconds between fetches. Omit to fetch once per run. |
| `scrape_timeout_seconds` | integer | `5` | Request timeout in seconds, including reading the body. |
| `format` | string | `json` | How the response is parsed: `json`, `csv`, or `xml`. |
| `batch_size_bytes` | integer | `1024000` | Multi-line responses larger than this are split on line boundaries into chunks of about this size. Newline-delimited JSON splits cleanly. For `csv`, `xml`, or a pretty-printed JSON array, set it above the largest response so the whole document stays together. |
| `batch_size_seconds` | integer | — | Not used by this connector. |

### Environment variables

| Key | Type | Default | Description |
|---|---|---|---|
| `SKIPPR_RUNTIME_ONCE_IDLE_TIMEOUT_SECONDS` | integer | `60` | With `skipprd sync --once` and `scrape_interval_seconds` set, seconds without new data before the run finishes. |

## What gets synced

**Records.** The response body is parsed with `format`. With the default `format: json`, a JSON object becomes one record, a JSON array becomes one record per element, and newline-delimited JSON becomes one record per line. With `format: csv`, Skipprd detects the delimiter and reads the header from the first line.

**Namespace.** All records land in the namespace `http`.

**Delivery.** Each fetch lands the entire response. Skipprd doesn't remember earlier responses, so polling an endpoint that returns the same items again lands them again. Deduplicate downstream, or poll an endpoint that returns only new items.

**Errors.** A response with a `4xx` or `5xx` status lands nothing. With `scrape_interval_seconds`, Skipprd logs the error and tries again at the next interval. Without it, the run fails.

**Restarts.** Without `scrape_interval_seconds`, every run fetches once. In continuous mode (`skipprd sync` without `--once`), that's every `sync_frequency_seconds` (900 seconds by default). See [Pipelines](/configuration/pipeline).

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `401` or `403` | Missing or wrong credentials, or an unsupported `auth.strategy`. | Set `auth.strategy` to `bearer` or `basic` and check the token or password variable. |
| `invalid environment reference` or `must be the entire scalar value` | A header like `Authorization: Bearer ${TOKEN}` mixes text and a reference. | Use `auth.strategy: bearer`, or export the whole header value and reference it as `${STATUS_API_AUTH}`. |
| Timeout errors | The endpoint takes longer than 5 seconds. | Raise `scrape_timeout_seconds`. |
| A large CSV, XML, or pretty-printed JSON response lands as broken records or deadletters | The response was split into chunks, so later chunks lack the header or the start of the document. | Raise `batch_size_bytes` above the response size. |
| Duplicate rows on every poll | The endpoint returns the full list each time. | Expected. Deduplicate on a key in your models, or lengthen `scrape_interval_seconds`. |

## Next steps

- [Destinations](/configuration/output): land the response in a warehouse.
- [Deadletters](/concepts/deadletters): where records that fail to parse go.
- [Connector catalog](/connectors/): connectors for paginated APIs.
