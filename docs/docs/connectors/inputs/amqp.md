# AMQP Input

Consumes messages from an AMQP queue (RabbitMQ compatible).

## How it works

1. Connects to the AMQP broker and declares the queue.
2. Consumes messages with configurable prefetch count.
3. Messages are acknowledged after successful ingest.
4. Supports stream and batch modes.
5. Namespace convention: `amqp.{queue}`.

## Configuration

```yaml
data_sources:
  source:
    Amqp:
      connection_string: "amqp://guest:guest@localhost:5672"
      queue: events
      mode: batch
      prefetch_count: 20
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `connection_string` | *(required)* | AMQP connection URI |
| `queue` | *(required)* | Queue name |
| `exchange` | | Exchange to bind to |
| `routing_key` | | Routing key for binding |
| `consumer_tag` | auto-generated | Consumer tag |
| `prefetch_count` | `10` | Prefetch count |
| `mode` | `stream` | `stream` or `batch` |
| `idle_timeout_seconds` | `5` | Batch mode idle timeout |
| `format` | `json` | Data format |
