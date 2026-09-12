# Kafka Input

Consumes messages from a Kafka topic.

## How it works

1. Creates a Kafka consumer with the configured group ID.
2. Subscribes to the topic and begins consuming.
3. Offsets are committed after successful ingest.
4. Supports stream and batch modes.
5. Namespace convention: `kafka.{topic}`.

## Configuration

```yaml
data_sources:
  source:
    Kafka:
      brokers: "localhost:9092"
      topic: events
      group_id: skippr-consumer
      mode: batch
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `brokers` | *(required)* | Kafka bootstrap servers |
| `topic` | *(required)* | Topic to consume |
| `group_id` | auto-generated | Consumer group ID |
| `auto_offset_reset` | `earliest` | `earliest` or `latest` |
| `security_protocol` | | Security protocol |
| `sasl_mechanism` | | SASL mechanism |
| `sasl_username` / `sasl_password` | | SASL credentials |
| `mode` | `stream` | `stream` or `batch` |
| `idle_timeout_seconds` | `5` | Batch mode idle timeout |
| `format` | `json` | Data format |

## Authentication

Kafka supports unauthenticated local development as well as secured broker setups. For security best practices, we strongly advise against storing SASL credentials in `skippr.yml`. Use environment variable interpolation instead: replace the `sasl_username` or `sasl_password` value with your own `${ENV_VAR}` reference.

The relevant part of `skippr.yml` looks like this:

```yaml
data_sources:
  source:
    Kafka:
      security_protocol: SASL_SSL
      sasl_mechanism: PLAIN
      sasl_username: "${KAFKA_SASL_USERNAME}"
      sasl_password: "${KAFKA_SASL_PASSWORD}"
```

Set the env vars before running `skipprd`:

macOS / Linux

```bash
export KAFKA_SASL_USERNAME="myuser"
export KAFKA_SASL_PASSWORD="mypassword"
```

Windows PowerShell

```powershell
$env:KAFKA_SASL_USERNAME = "myuser"
$env:KAFKA_SASL_PASSWORD = "mypassword"
```

Windows Command Prompt

```cmd
set KAFKA_SASL_USERNAME=myuser
set KAFKA_SASL_PASSWORD=mypassword
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| connection or SASL failures | Verify broker addresses, security protocol, SASL settings, and network access to the cluster. |
| messages are not arriving | Check the topic name, consumer group, and whether the producer is publishing to the expected topic. |
