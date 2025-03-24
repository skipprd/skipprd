# Core Concepts


## Skippr Architecture

![Skippr Architecture](skiprd_and_enterprise.png "Skippr Architecture")

## At-least Once

Skippr guarantees at-least once delivery to the output. Checkpoints of ingested data from the input are tracked internally and committed once the data is synced to the output.

**Exactly Once** semantics are in development where the output plugins will commit offsets on sync.

## Checkpointing

Each input plugin implements its own checkpointing method according to the semantics of the data source.

For data sources like Kafka, this is simply a case of implementing the offsets capability of the Kafka Consumer API.

A more complex example of checkpointing can be found in the S3 input plugin, which tracks the object store last modified data and file line ingested.


## Formats, Serializers and Deserializers

Skippr supports various common data input and output formats. Universal (de)serialization is central to Skipprs `connect anything to anything` principle.

Skippr serde's are generally wrappers around common C libs with extra validation and **formatting auto-fixing**.

### Supported Formats

* Json
* Delimiter (`,` `;` `|` `\t`)
* Avro
* Parquet
* Arrow (Coming Soon)


## Auto-fixing

**... what do you mean by that?**

The lessons from hundreds of data sources, trillions of records and years of (frankly painful) data integration have been distilled into the auto-fixing serde's.

Whether it's unicode json, missing or incorrect quotes, syntax errors or magic-byte padding... Skippr will do it's level best to auto-fix on ingest avoiding dozens of common causes of broken pipelines and hours of wasted engineering hours.

## Skippr State Storage

Skippr creates and maintains the state file `skippr-state.json` in your state backend (e.g. local docker `/data` volume). This file contains Skippr internal metadata such as data source schema and checkpoints for ingestion progress (think kafka offsets or logstash sincedb).

NOTE: no source data or secrets are stored in the state.

## Skippr File Buffer

Skippr manages temporary file buffers while ingesting data. The default buffer backend is `file` located your local volume mount (e.g. `~/demo/buffer` directory).</p>
