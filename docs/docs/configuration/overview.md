# Configuration Overview

Skippr is configured entirely via environment variables (or an optional config file set via `SKIPPR_CONFIG_FILE`).

## Quick reference

| Variable | Default | Section | Description |
|---|---|---|---|
| **Pipeline identity** | | | |
| `PIPELINE_NAME` | `default` | [Pipeline](pipeline.md) | Pipeline name |
| `WORKSPACE_NAME` | `default` | [Pipeline](pipeline.md) | Workspace/domain name |
| `TENANT` | `default` | [Pipeline](pipeline.md) | Tenant identifier |
| **Input source** | | | |
| `DATA_SOURCE_PLUGIN_NAME` | *(required)* | [Input](input.md) | Source plugin: `s3`, `file` |
| `DATA_SOURCE_S3_BUCKET` | | [Input](input.md) | S3 source bucket |
| `DATA_SOURCE_S3_PREFIX` | | [Input](input.md) | S3 source key prefix |
| `DATA_SOURCE_PATH` | | [Input](input.md) | Local file source path |
| `DATA_SOURCE_BATCH_SIZE_BYTES` | plugin-defined | [Input](input.md) | Batch size in bytes |
| `DATA_SOURCE_BATCH_SIZE_SECONDS` | plugin-defined | [Input](input.md) | Batch size in seconds |
| **Output destination** | | | |
| `DATA_OUTPUT_S3_BUCKET` | | [Output](output.md) | Destination S3 bucket for Parquet |
| `DATA_OUTPUT_S3_PREFIX` | | [Output](output.md) | Destination S3 key prefix |
| `SCHEMA_OUTPUT_GLUE_DATABASE_NAME` | | [Output](output.md) | Glue catalog database name |
| `DATA_OUTPUT_ATHENA_WORKGROUP_NAME` | | [Output](output.md) | Athena workgroup |
| `DATA_OUTPUT_ATHENA_RESULTS_S3_BUCKET` | | [Output](output.md) | S3 bucket for Athena query results |
| `DATA_OUTPUT_MAX_ASYNC_UPLOADS` | `16` | [Output](output.md) | Max concurrent Parquet uploads |
| **Transforms** | | | |
| `TRANSFORM_NAMESPACE_FIELDS` | | [Transforms](transforms.md) | Fields that define event type / namespace |
| `TRANSFORM_BATCH_PARTITION_FIELDS` | | [Transforms](transforms.md) | Fields for Hive partitioning |
| `TRANSFORM_BATCH_TIME_FIELDS` | | [Transforms](transforms.md) | Timestamp field(s) for time partitioning |
| `TRANSFORM_BATCH_TIME_UNIT` | | [Transforms](transforms.md) | Time granularity: `year`, `month`, `day`, `hour`, `minute` |
| `TRANSFORM_FLATTEN_EVENTS` | `no` | [Transforms](transforms.md) | Flatten nested structures |
| `TRANSFORM_BATCH_ORDER_FIELDS` | | [Transforms](transforms.md) | Sort rows within Parquet files for predicate pruning |
| **Buffering & WAL** | | | |
| `BUFFER_THRESHOLD_BYTES` | `10485760` | [Buffering](buffering.md) | Buffer flush threshold (bytes) |
| `BUFFER_THRESHOLD_SECONDS` | `60` | [Buffering](buffering.md) | Buffer flush threshold (seconds) |
| `WAL_STORAGE` | `disk` | [Buffering](buffering.md) | WAL backend: `disk` or `s3` |
| `WAL_BYTES_PER_FILE` | `4194304` | [Buffering](buffering.md) | Target WAL segment size (bytes) |
| `WAL_MAX_DELAY_SECONDS` | `60` | [Buffering](buffering.md) | Max WAL segment age before flush |
| **Skippr state** | | | |
| `SKIPPR_S3_BUCKET` | | [Advanced](advanced.md) | S3 bucket for metadata, offsets, WAL (when S3), deadletters |
| `SKIPPR_STORAGE_MODE` | `s3` | [Advanced](advanced.md) | Metadata and stats persistence: `s3` (default) or `local` |
| `DATA_DIR` | `./data` | [Advanced](advanced.md) | Local directory for WAL segments and offsets DB |
| **Operational** | | | |
| `SKIPPR_CHAOS_MODE` | `no` | [Advanced](advanced.md) | Enable chaos mode (random SIGKILL for testing) |
| `SKIPPR_ENV` | `prod` | [Advanced](advanced.md) | Environment label |
| `SCHEMA_AUTO_APPROVE` | `true` | [Advanced](advanced.md) | Auto-approve schema changes |
| `RESET_OFFSETS` | `false` | [Advanced](advanced.md) | Reset offsets on startup |
| `RESET_METADATA` | `false` | [Advanced](advanced.md) | Reset metadata on startup |
| `SYNC_FREQUENCY` | | [Advanced](advanced.md) | Sync frequency (seconds) |
