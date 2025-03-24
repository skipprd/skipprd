# Kafka Input Plugin

Follows all the usual Kafka semantics:

- one message/event per kafka record
- one schema type per kafka topic

##### Data Formats

- json
- avro

See: [Skippr serialisation formats](/formats)

### Config

```bash
DATA_SOURCE_PLUGIN_NAME: kafka
DATA_SOURCE_BROKERS: kafka:9092
DATA_SOURCE_TOPIC: [mytopic]
DATA_SOURCE_KAFKA_CONFIG: consumer.example1=foo,consumer.example2=bar
DATA_SOURCE_FORMAT: json|avro
SCHEMA_REGISTRY: hostname:8080
DATA_SOURCE_REGISTRY_API_TOKEN: [SECRET]
```

#### Consumer Configurations (Optional)

Any [Kafka Consumer Configs](https://kafka.apache.org/documentation/#consumerconfigs) can be set as a comma separated list of `key=value` pairs on  `DATA_SOURCE_KAFKA_CONFIG`.

For example:

```
DATA_SOURCE_KAFKA_CONFIG: fetch.min.bytes=1000,group.instance.id=foo
```

##### Schema Registry

The Skippr Kafka plugin optionally supports a Confluent compliant Avro schema registries, simply supply the `SCHEMA_REGISTRY` environment variable.

Skippr Enterprise provides a Confluent compliant schema registry with the added feature of authentication (set the `DATA_SOURCE_REGISTRY_API_TOKEN` environment variable). It is not required to use the Skippr schema registry or indeed any registry.


### Scaling

As with any Kafka consumer, the Skippr Kafka source will scale upto one worker per topic partition.
