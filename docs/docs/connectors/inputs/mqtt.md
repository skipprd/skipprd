# MQTT Input

Subscribes to an MQTT topic and ingests messages. Supports stream and batch modes.

## How it works

1. Connects to the MQTT broker and subscribes to the configured topic.
2. **Stream mode (default):** continuously receives messages until shutdown.
3. **Batch mode:** drains available messages and exits after `idle_timeout_seconds` of no new messages.
4. Namespace convention: `mqtt.{topic}`.

## Configuration

```yaml
data_sources:
  source:
    Mqtt:
      broker_url: "mqtt.example.com"
      port: 1883
      topic: "sensors/temperature"
      mode: batch
      idle_timeout_seconds: 10
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `broker_url` | *(required)* | MQTT broker hostname |
| `port` | `1883` | Broker port |
| `topic` | *(required)* | Topic to subscribe to |
| `client_id` | auto-generated | MQTT client ID |
| `qos` | `1` | Quality of Service (0, 1, 2) |
| `username` / `password` | | Optional broker credentials |
| `mode` | `stream` | `stream` or `batch` |
| `idle_timeout_seconds` | `5` | Batch mode idle timeout |
| `format` | `json` | Data format |

## Authentication

Authentication depends on the broker. Use `username` and `password` when the broker requires credentials, or omit them for local development.

## Troubleshooting

| Symptom | Fix |
|---|---|
| connection refused | Verify the broker URL, port, TLS requirements, and network access from the runner. |
| no messages arriving | Check the topic name, QoS, and whether the broker ACLs allow subscriptions for this client. |
