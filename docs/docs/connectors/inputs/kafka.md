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
