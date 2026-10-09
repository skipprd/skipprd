---
title: MQTT source
description: Subscribe Skipprd to an MQTT topic or wildcard and land each published message as a record while the pipeline runs.
---

# MQTT source

The MQTT source connects to an MQTT broker, subscribes to one topic filter, and lands each published message as one or more records. Use it to capture IoT sensor readings, device telemetry, or other pub/sub traffic in a warehouse. MQTT has no replay, so Skipprd captures only what is published while it is connected.

## Before you begin

You need:

- Network access from the machine running Skipprd to the broker's plain MQTT port, usually `1883`.
- The broker hostname, for example `mqtt.example.com`. Use the hostname only, not a `mqtt://` URL.
- A username and password if the broker requires them, with an ACL that allows subscribing to your topic.

::: warning No TLS
The MQTT source connects without TLS. To reach a broker that only accepts TLS (port `8883`), run Skipprd on a network where a plain listener is available, for example through a local bridge.
:::

## Configure

Keep the broker password in an environment variable. `skippr.yml` stores only the reference.

```bash
export MQTT_PASSWORD="your-password"
```

::: code-group

```python [Python]
from skippr import Config, DataSourceMqtt, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "sensors",
    DataSourceMqtt(
        broker_url="mqtt.example.com",
        port=1883,
        topic="sensors/+/temperature",
        username="skippr",
        password=EnvRef("MQTT_PASSWORD"),
        client_id="skippr-sensors",
    ),
)
cfg.pipeline("sensors", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source mqtt \
  --pipeline sensors \
  --name sensors \
  --broker-url mqtt.example.com \
  --port 1883 \
  --topic 'sensors/+/temperature' \
  --username skippr \
  --password '${MQTT_PASSWORD}' \
  --client-id skippr-sensors
```

```yaml [YAML]
pipelines:
  sensors:
    data_source: data_sources.sensors

data_sources:
  sensors:
    Mqtt:
      broker_url: mqtt.example.com
      port: 1883
      topic: "sensors/+/temperature"
      username: skippr
      password: "${MQTT_PASSWORD}"
      client_id: skippr-sensors
```

:::

Run the pipeline and check the result:

1. Start the sync. Without `--once`, Skipprd stays subscribed until you stop it.

   ```bash
   skipprd sync --pipeline sensors --log
   ```

2. Publish a test message, for example with the Mosquitto client:

   ```bash
   mosquitto_pub -h mqtt.example.com -u skippr -P "$MQTT_PASSWORD" \
     -t sensors/room-1/temperature -m '{"room": "room-1", "celsius": 21.5}'
   ```

3. Read what landed:

   ```bash
   skipprd df --pipeline sensors
   ```

To load the data into a warehouse, add a destination to the pipeline. See [Destinations](/configuration/output).

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `broker_url` | string | Required | Broker hostname or IP address, without a scheme. |
| `topic` | string | Required | Topic filter to subscribe to. MQTT wildcards `+` and `#` work. |
| `port` | integer | `1883` | Broker port. |
| `client_id` | string | `skippr-<random>` | MQTT client ID. Set a fixed value so broker logs and ACLs can identify Skipprd. |
| `qos` | integer | `1` | Subscription quality of service: `0` (at most once), `1` (at least once), or `2` (exactly once). Any other value is treated as `1`. |
| `username` | string | — | Broker username. Used only when `password` is also set. |
| `password` | secret | — | Broker password as a `${NAME}` reference. Used only when `username` is also set. |
| `mode` | string | `stream` | `stream` stays subscribed until Skipprd stops. `batch` stops after `idle_timeout_seconds` with no new message. |
| `idle_timeout_seconds` | integer | `5` | In `batch` mode, seconds without a message before the source finishes. |
| `format` | string | `json` | How each message payload is parsed: `json`, `csv`, or `xml`. |
| `batch_size_bytes` | integer | — | Not used by this connector. Each message is written individually; tune landing batches with the pipeline [buffer thresholds](/configuration/buffering). |
| `batch_size_seconds` | integer | — | Not used by this connector. |

### Environment variables

| Key | Type | Default | Description |
|---|---|---|---|
| `SKIPPR_RUNTIME_ONCE_IDLE_TIMEOUT_SECONDS` | integer | `60` | With `skipprd sync --once` in `stream` mode, seconds without a new message before the run finishes. |

## What gets synced

**Records.** Skipprd reads the message payload as UTF-8 text and ignores the topic name of each individual message, so records from a wildcard subscription don't say which topic they came from. Put identifying fields, such as a device ID, in the payload. With the default `format: json`, a JSON object becomes one record and a JSON array becomes one record per element.

**Namespace.** All records land in the namespace `mqtt.<topic>`, using the topic filter exactly as configured, for example `mqtt.sensors/+/temperature`.

**Delivery.** The broker considers a message delivered as soon as Skipprd receives it, before it is written to the write-ahead log (WAL). A message received just before a crash can be lost. MQTT can't replay messages, so treat this source as best effort and use [Kafka](/connectors/inputs/kafka) or a queue when you need guaranteed delivery.

**Ordering.** Skipprd processes messages one at a time, in the order the broker delivers them.

**Restarts.** Skipprd connects with a clean session, so messages published while it is stopped are not delivered when it reconnects. If the connection to the broker drops, the source stops; in continuous mode, the next sync loop reconnects.

**Schema discovery.** The first time a pipeline runs, Skipprd discovers the schema from live messages. `skipprd sync` does this automatically if you haven't run `skipprd discover`. Messages received during discover aren't landed.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `connection refused` | Wrong host or port, or the broker only accepts TLS. | Check `broker_url` and `port`. Use the broker's plain listener. |
| Address or DNS lookup errors | `broker_url` includes a scheme or path, such as `mqtt://mqtt.example.com`. | Set `broker_url` to the bare hostname. |
| Broker rejects the connection as not authorized | Credentials are missing or wrong. | Set both `username` and `password`; Skipprd sends credentials only when both are present. |
| Connected, but no records | The ACL blocks the subscription, or nothing publishes to the filter. | Check the broker ACL for the client and test with `mosquitto_sub` using the same credentials and topic. |
| Messages missing after a restart | Clean sessions don't keep messages for disconnected clients. | Keep the pipeline running continuously, or publish to a queue Skipprd can drain. |
| Another client is disconnected when Skipprd connects | Two clients share one `client_id`. | Give each pipeline a unique `client_id`. |

## Next steps

- [Destinations](/configuration/output): land the topic in a warehouse.
- [Kafka source](/connectors/inputs/kafka): replayable streaming with committed offsets.
- [WAL and buffering](/configuration/buffering): control how often batches land.
