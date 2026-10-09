---
title: Kafka source
description: Consume a Kafka topic with Skipprd and land each message as a record, with consumer-group offsets committed only after data is durable.
---

# Kafka source

The Kafka source joins a consumer group, reads one topic, and lands each message value as one or more records. Use it to load event streams, application logs, or Debezium change events from Kafka into your warehouse. Skipprd commits the group's offset only after a message is safely in its write-ahead log (WAL), so a crash never skips data.

## Before you begin

You need:

- Network access from the machine running Skipprd to every broker in `brokers`.
- A topic that producers write to, and permission for your user to read it and to commit offsets for the consumer group.
- If the cluster uses SASL, a username and password for the `PLAIN` mechanism.

::: warning TLS is not available in current builds
Skipprd connects with `security_protocol` `PLAINTEXT` or `SASL_PLAINTEXT`. Clusters that require TLS (`SSL` or `SASL_SSL`), such as most managed Kafka services, can't be reached directly yet.
:::

## Configure

Store the SASL password in an environment variable. Skipprd reads it when the run starts, and `skippr.yml` keeps only the reference.

```bash
export KAFKA_SASL_PASSWORD="your-password"
```

::: code-group

```python [Python]
from skippr import Config, DataSourceKafka, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "orders_topic",
    DataSourceKafka(
        brokers="broker-1:9092,broker-2:9092",
        topic="orders",
        group_id="skippr-orders",
        security_protocol="SASL_PLAINTEXT",
        sasl_mechanism="PLAIN",
        sasl_username="skippr",
        sasl_password=EnvRef("KAFKA_SASL_PASSWORD"),
    ),
)
cfg.pipeline("orders", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source kafka \
  --pipeline orders \
  --name orders_topic \
  --brokers broker-1:9092,broker-2:9092 \
  --topic orders \
  --group-id skippr-orders \
  --security-protocol SASL_PLAINTEXT \
  --sasl-mechanism PLAIN \
  --sasl-username skippr \
  --sasl-password '${KAFKA_SASL_PASSWORD}'
```

```yaml [YAML]
pipelines:
  orders:
    data_source: data_sources.orders_topic

data_sources:
  orders_topic:
    Kafka:
      brokers: "broker-1:9092,broker-2:9092"
      topic: orders
      group_id: skippr-orders
      security_protocol: SASL_PLAINTEXT
      sasl_mechanism: PLAIN
      sasl_username: skippr
      sasl_password: "${KAFKA_SASL_PASSWORD}"
```

:::

For a local broker without authentication, set only `brokers` and `topic`.

Run the pipeline and check the result:

1. Produce a few test messages to the topic. The first run discovers the schema from live messages (see [What gets synced](#what-gets-synced)).
2. Start the sync. Without `--once`, Skipprd keeps consuming until you stop it.

   ```bash
   skipprd sync --pipeline orders --log
   ```

3. In another terminal, read what landed:

   ```bash
   skipprd df --pipeline orders
   ```

To load the data into a warehouse, add a destination to the pipeline. See [Destinations](/configuration/output).

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `brokers` | string | Required | Comma-separated bootstrap servers, for example `broker-1:9092,broker-2:9092`. |
| `topic` | string | Required | Topic to consume. One topic per source. |
| `group_id` | string | `skippr-<topic>` | Consumer group. Offsets are committed to this group, so a new `group_id` starts over from `auto_offset_reset`. |
| `auto_offset_reset` | string | `earliest` | Where a group with no committed offset starts: `earliest` (oldest retained message) or `latest` (only new messages). |
| `security_protocol` | string | `PLAINTEXT` | `PLAINTEXT` or `SASL_PLAINTEXT`. |
| `sasl_mechanism` | string | — | `PLAIN` when the cluster uses SASL. |
| `sasl_username` | string | — | SASL username. |
| `sasl_password` | secret | — | SASL password as a `${NAME}` reference. |
| `mode` | string | `stream` | `stream` keeps consuming until Skipprd stops. `batch` stops after `idle_timeout_seconds` with no new message, which suits scheduled jobs that drain a backlog. |
| `idle_timeout_seconds` | integer | `5` | In `batch` mode, seconds without a message before the source finishes. |
| `format` | string | `json` | How each message value is parsed: `json`, `csv`, or `xml`. See [What gets synced](#what-gets-synced). |
| `cdc_mode` | string | `snapshot` | `snapshot` lands messages as plain appended records. `cdc_only` attaches change metadata to each record for [change data capture](/cdc/). `snapshot_then_cdc` is rejected because Kafka has no snapshot to read. |
| `debezium_format` | boolean | `false` | With `cdc_mode: cdc_only`, unwrap Debezium change envelopes. See [Debezium change events](#debezium-change-events). |
| `batch_size_bytes` | integer | — | Not used by this connector. Each message is written individually; tune landing batches with the pipeline [buffer thresholds](/configuration/buffering). |
| `batch_size_seconds` | integer | — | Not used by this connector. |

### Environment variables

| Key | Type | Default | Description |
|---|---|---|---|
| `SKIPPR_RUNTIME_ONCE_IDLE_TIMEOUT_SECONDS` | integer | `60` | With `skipprd sync --once` in `stream` mode, seconds without a new message before the run finishes. Raise it if producers are bursty. |

## What gets synced

**Records.** Skipprd reads the message value and ignores the key, headers, and timestamp. With the default `format: json`:

- a JSON object becomes one record;
- a JSON array becomes one record per element;
- newline-delimited JSON becomes one record per line.

Messages with an empty value (tombstones) are skipped. With `format: csv`, each message is parsed as its own CSV document, so include the header row in every message.

**Namespace.** All records land in the namespace `kafka.<topic>`, for example `kafka.orders`. Your destination turns the namespace into a table name.

**Delivery.** Skipprd commits a message's offset only after the message is durable in the WAL. If Skipprd stops between those two steps, the next run reads that message again, so delivery is at least once. Design downstream models to tolerate an occasional duplicate, or use `cdc_mode: cdc_only` so each record carries a stable event ID.

**Ordering.** Skipprd consumes each partition in offset order. Warehouse tables don't keep arrival order, so include an event timestamp in your messages if order matters.

**Restarts.** A restarted pipeline resumes from the consumer group's last committed offset. Changing `group_id` starts a new group at `auto_offset_reset`.

**Schema discovery.** The first time a pipeline runs, Skipprd discovers the schema by reading messages from the topic. `skipprd sync` does this automatically if you haven't run `skipprd discover`. Discover commits the offsets of the messages it reads but doesn't land them, so run it against test messages or a test consumer group.

### Debezium change events

Set `cdc_mode: cdc_only` and `debezium_format: true` to consume a Debezium topic. For each envelope, Skipprd reads `op` and lands:

| `op` | Lands as | Row data |
|---|---|---|
| `c` | insert | `after` |
| `u` | update | `after` |
| `d` | delete | `before` |
| `r` | snapshot read | `after` |

Each record's event ID is its topic, partition, and offset. A message that isn't valid JSON lands unchanged as an insert.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Errors mention `SSL` or `SASL_SSL` being unsupported | Current builds don't include TLS for Kafka. | Use a listener that accepts `PLAINTEXT` or `SASL_PLAINTEXT`, for example through a private network. |
| Authentication fails | Wrong mechanism or credentials. | Set `sasl_mechanism: PLAIN` and check `sasl_username` and the `KAFKA_SASL_PASSWORD` value. |
| `environment variable is not set` | The `${KAFKA_SASL_PASSWORD}` reference can't be resolved. | Export the variable in the shell or service that runs `skipprd`. |
| No records, no errors | The group already committed past the messages, or `auto_offset_reset: latest` skips the backlog. | Use a new `group_id` with `auto_offset_reset: earliest` to re-read retained messages. |
| `snapshot_then_cdc` error at start | Kafka has no snapshot API. | Use `cdc_mode: cdc_only`. |
| Run with `--once` never finishes | `mode: stream` waits for the idle timeout while messages keep arriving. | Use `mode: batch` with `idle_timeout_seconds`, or run without `--once` as a long-lived process. |

## Next steps

- [Destinations](/configuration/output): land the topic in a warehouse.
- [Change data capture](/cdc/): how CDC records converge in the destination.
- [WAL and buffering](/configuration/buffering): control how often batches land.
