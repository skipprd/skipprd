# HTTP Input

Downloads data from a URL via HTTP `GET`.

## Supported formats

- Depends on the `format` option and response content (e.g. JSON lines, CSV). Responses with `Content-Encoding: gzip` or `.gz` URLs are decompressed automatically.

## How it works

1. Performs an HTTP `GET` against the configured URL.
2. Streams the response body into the ingestion pipeline.
3. Applies optional gzip decompression when indicated by headers or URL.
4. Namespace convention: `http` (single logical source).

## Configuration

```bash
DATA_SOURCE_PLUGIN_NAME=Http
DATA_SOURCE_HTTP_URL=https://example.com/data.jsonl.gz
```

Or via YAML pipeline config:

```yaml
data_sources:
  source:
    Http:
      url: "https://example.com/data.jsonl"
      format: jsonl
      batch_size_bytes: 1048576
      batch_size_seconds: 60
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `DATA_SOURCE_HTTP_URL` | *(required)* | URL to fetch |
| `url` | | URL (YAML) |
| `format` | | Optional format hint for the parser |
| `batch_size_bytes` | | Override WAL segment size in bytes |
| `batch_size_seconds` | | Override WAL segment flush interval in seconds |

## Namespace convention

```
http
```

All records from this source share the `http` namespace.
