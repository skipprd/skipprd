# Kafka Output Plugin

Implements a Kafka Producer client with optional support for a Confluent compliant Schema Registry.

##### Data Formats

- json
- avro

See: [Skippr serialisation formats](/formats)

### Config

```bash
DATA_OUTPUT_PLUGIN_NAME: kafka
DATA_OUTPUT_BROKERS: kafka:9092
DATA_OUTPUT_TOPIC: [mytopic]
DATA_OUTPUt_KAFKA_CONFIG: consumer.example1=foo,consumer.example2=bar
DATA_OUTPUT_FORMAT: json|avro
SCHEMA_REGISTRY: hostname:8080
DATA_OUTPUT_REGISTRY_API_TOKEN: [SECRET]
```

#### Producer Configurations (Optional)

Any [Kafka Producer Configs](https://kafka.apache.org/documentation/#producerconfigs) can be set as a comma separated list of `key=value` pairs on  `DATA_OUTPUT_KAFKA_CONFIG`.

For example:

```
DATA_OUTPUT_KAFKA_CONFIG: compression.codec=zstd,batch.num.messages=1000000
```

##### Schema Registry

The Skippr Kafka plugin optionally supports a Confluent compliant Avro schema registries, simply supply the `SCHEMA_REGISTRY` environment variable.

Skippr Enterprise provides a Confluent compliant schema registry with the added feature of authentication (set the `DATA_OUTPUT_REGISTRY_API_TOKEN` environment variable). It is not required to use the Skippr schema registry or indeed any registry.
