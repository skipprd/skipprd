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

## Authentication

Authentication is provided through the AMQP connection URI. For security best practices, we strongly advise against storing the connection string in `skippr.yml`. Use environment variable interpolation instead: replace the `connection_string` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sinks:
  warehouse:
    Amqp:
      connection_string: "${AMQP_CONNECTION_STRING}"
```

Set the env var before running `skipprd`:

macOS / Linux

```bash
export AMQP_CONNECTION_STRING="amqp://guest:guest@localhost:5672"
```

Windows PowerShell

```powershell
$env:AMQP_CONNECTION_STRING = "amqp://guest:guest@localhost:5672"
```

Windows Command Prompt

```cmd
set AMQP_CONNECTION_STRING=amqp://guest:guest@localhost:5672
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| authentication or connection failures | Verify the AMQP URI, broker hostname, port, and vhost permissions. |
| messages are not routed | Check the exchange name, exchange type, routing key, and downstream bindings. |
