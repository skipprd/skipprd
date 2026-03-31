# AMQP Output

Publishes records as JSON messages to an AMQP exchange (RabbitMQ compatible).

## How it works

1. Connects to the AMQP broker and declares the exchange.
2. Each row from the record batch is serialized to JSON and published.

## Configuration

```yaml
data_sinks:
  sink:
    Amqp:
      connection_string: "amqp://guest:guest@localhost:5672"
      exchange: events
      routing_key: output
      exchange_type: direct
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `connection_string` | *(required)* | AMQP connection URI |
| `exchange` | *(required)* | Exchange name |
| `routing_key` | `""` | Routing key |
| `exchange_type` | `direct` | Exchange type (direct, fanout, topic, headers) |
| `format` | `json` | Data format |
