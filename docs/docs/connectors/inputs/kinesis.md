---
title: Kinesis source
description: Read every shard of an Amazon Kinesis data stream with Skipprd, resuming from a per-shard checkpoint after each run.
---

# Kinesis source

The Kinesis source reads every shard of an Amazon Kinesis data stream and lands each record's data as one or more records. By default it reads until every shard is caught up and then finishes, which suits scheduled loads; set `mode: stream` to keep reading. Skipprd saves its position in each shard after the data is safely in its write-ahead log (WAL), so the next run continues where the last one stopped.

## Before you begin

You need:

- The stream name and its region.
- AWS credentials that Skipprd can find through the standard AWS credential chain: environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`), a shared profile, or an instance, task, or pod role.
- An IAM policy that allows Skipprd to list shards and read records:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "kinesis:ListShards",
        "kinesis:GetShardIterator",
        "kinesis:GetRecords"
      ],
      "Resource": "arn:aws:kinesis:us-east-1:123456789012:stream/clickstream"
    }
  ]
}
```

If the stream uses server-side encryption with a customer-managed KMS key, also allow `kms:Decrypt` on that key.

## Configure

Any standard AWS credential source works. For example, use a named profile:

```bash
export AWS_PROFILE=analytics
```

::: code-group

```python [Python]
from skippr import Config, DataSourceKinesis, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "clicks",
    DataSourceKinesis(stream_name="clickstream", region="us-east-1"),
)
cfg.pipeline("clickstream", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source kinesis \
  --pipeline clickstream \
  --name clicks \
  --stream-name clickstream \
  --region us-east-1
```

```yaml [YAML]
pipelines:
  clickstream:
    data_source: data_sources.clicks

data_sources:
  clicks:
    Kinesis:
      stream_name: clickstream
      region: us-east-1
```

:::

Run the pipeline and check the result:

1. Read the stream once, up to the latest record in every shard:

   ```bash
   skipprd sync --pipeline clickstream --once --log
   ```

2. Read what landed:

   ```bash
   skipprd df --pipeline clickstream
   ```

To load the data into a warehouse, add a destination to the pipeline. See [Destinations](/configuration/output).

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `stream_name` | string | Required | Kinesis data stream name. |
| `region` | string | AWS credential chain | AWS region, for example `us-east-1`. Falls back to `AWS_REGION` or your profile. |
| `endpoint_url` | string | — | Custom Kinesis endpoint, for example `http://localhost:4566` for LocalStack. |
| `mode` | string | `batch` | `batch` reads until no shard returns records and every shard reports it is caught up, then finishes. `stream` keeps reading, backing off from 100 ms up to 5 seconds while the stream is idle. |
| `format` | string | `json` | How each record's data is parsed: `json`, `csv`, or `xml`. |
| `batch_size_bytes` | integer | `1024000` | Per shard, Skipprd writes buffered records to the WAL and saves its shard position each time this many bytes accumulate, and whenever a read round returns nothing. |
| `batch_size_seconds` | integer | — | Not used by this connector. |

### Environment variables

| Key | Type | Default | Description |
|---|---|---|---|
| `DATA_DIR` | string | `./data` | Pipeline data directory (the pipeline's `data_dir` in `skippr.yml` takes precedence). Skipprd keeps each shard's saved position here, next to the WAL. Put it on persistent storage. |
| `SKIPPR_RUNTIME_ONCE_IDLE_TIMEOUT_SECONDS` | integer | `60` | With `skipprd sync --once` in `stream` mode, seconds without a new record before the run finishes. |

## What gets synced

**Records.** Skipprd reads each Kinesis record's data blob as UTF-8 text and ignores the partition key and arrival timestamp. With the default `format: json`, a JSON object becomes one record, a JSON array becomes one record per element, and newline-delimited JSON becomes one record per line.

**Namespace.** All records land in the namespace `kinesis.<stream_name>`, for example `kinesis.clickstream`.

**Delivery.** After a batch from a shard is durable in the WAL, Skipprd saves the sequence number of the last record in that batch. If Skipprd stops after writing but before saving, the next run re-reads those records, so delivery is at least once.

**Ordering.** Records within a shard are read in sequence-number order. Skipprd reads all shards in turn, so records from different shards interleave. Include an event timestamp if order matters.

**Restarts.** The first run starts at the oldest record the stream still retains (trim horizon). Later runs continue after each shard's saved position. The positions live in the pipeline data directory: if you delete it, or run on a new machine without it, Skipprd starts again from the oldest retained record. Skipprd lists shards when the run starts, so shards created by resharding are picked up on the next run.

**Schema discovery.** The first time a pipeline runs, Skipprd discovers the schema by reading records from the stream. `skipprd sync` does this automatically if you haven't run `skipprd discover`. Discover saves shard positions for the records it reads but doesn't land them, so run it against a test stream or test records.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `AccessDenied` on `ListShards`, `GetShardIterator`, or `GetRecords` | The IAM policy is missing an action. | Allow all three actions on the stream ARN. |
| `ResourceNotFoundException` | Wrong `stream_name` or `region`. | Check the name and region in the Kinesis console. |
| Every run re-reads the whole stream | The data directory isn't persistent, for example in a container without a volume. | Mount persistent storage and point `DATA_DIR` at it. |
| `ProvisionedThroughputExceededException` in the log | Other consumers share the shard's read limit. | Skipprd retries from its saved position. Reduce other consumers or add shards. |
| `batch` run finishes before reaching new data | Producers wrote after every shard reported it was caught up. | Run again, or use `mode: stream`. |

## Next steps

- [Destinations](/configuration/output): land the stream in a warehouse.
- [WAL and buffering](/configuration/buffering): control how often batches land.
- [Advanced settings](/configuration/advanced): data directory and other engine settings.
