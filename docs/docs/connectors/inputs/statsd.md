# StatsD Input

Listens for StatsD metrics over UDP and converts them to JSON.

## How it works

1. Binds a UDP socket on the configured address.
2. Parses incoming StatsD line protocol (`metric:value|type|@rate|#tags`).
3. Each metric is converted to a JSON object with `name`, `value`, `type`, `sample_rate`, and `tags` fields.
4. Namespace convention: `statsd`.

## Configuration

```yaml
data_sources:
  source:
    Statsd:
      listen_address: "0.0.0.0:8125"
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `listen_address` | `0.0.0.0:8125` | UDP address to listen on |
| `format` | `json` | Data format |
