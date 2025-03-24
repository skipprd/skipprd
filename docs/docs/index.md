# Welcome to Skippr Docs 👋


### What is Skippr

Skippr solves the EL in ELT, connecting any data source to any destination.

Specifically, Skippr discovers your source data schemas and converts your data to an optimised format for your output destinations.

**Examples:**

* json files on your local disk -> Avro messages in Kafka
* json files on S3 -> Parquet on S3

# Key Features

## Any Data Source, Any Destination

Skippr is an open platform, input and output plugins are open source and joyfully simple to create. All the complexity is transparently solved in skippr core, which the input and output plugins wrap as PHP bindings.

## Universal Data Format and Integration

CSV, Json, XML, Avro, Parquet, Arrow... think of Skippr as your effortless bridge to beyond data integration. Complete freedom to migrate data between any system, format and truly multi-cloud.

Because Skippr guarantees your schemas, you can output optimised data formats for your destination or even standardise on the zero-copy world of Apache Arrow.

## Schema Discovery and Evolution

Schema changes overtime are automatically handled, with Enterprise supporting backwards compatible evolution rules and propagation to destinations (Hive, Avro Registry, DBRM's etc)


## Getting Started

Running Skippr is as easy.

### Skippr Docker

Skippr docker solves data integration, just configure an input and output plugin and specify your desired output (e.g. json file to parquet on S3). Skippr auto-fixing serialisers and schema discovery algorithms empower you to focus on creating value from your data, not migrating it.

See [Getting Started Docker](getting-started/docker).

### Skippr Enterprise

Skippr Enterprise deploys as a serverless cloud solution, managing away complex pipeline challenges like auto-scaling, scheduling, dead-lettering and destination schema maintenance.

Deploy to your private cloud account with one command or as part of your CICD. See the [Getting Started Guide](enterprise/aws).




# Help

Join us on [Slack](https://join.slack.com/t/skipprgroup/shared_invite/zt-np4dqtsw-ASqYMtQlFkvDTWXhz4hxYw).
