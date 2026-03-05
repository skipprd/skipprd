# Deadletters

Records that fail validation or hit schema conflicts during ingestion are captured as deadletters rather than silently dropped.

## How deadletters work

- Deadletters are written directly to S3 as individual Parquet files — no WAL or compaction step
- A log line is emitted for each deadletter: `Deadletter id=<id> ns=<namespace> err=<first_error>`
- The raw record is preserved along with full error context and a snapshot of the namespace metadata at the time of failure

## S3 layout

```
s3://{SKIPPR_S3_BUCKET}/deadletters/{tenant}/{workspace}/{pipeline}/
  namespace={namespace}/
    p_year={YYYY}/
      p_month={MM}/
        p_day={DD}/
          {id}.parquet
```

## Parquet schema

Each deadletter Parquet file contains a single row with these columns:

| Column | Type | Description |
|---|---|---|
| `id` | string | Unique deadletter ID |
| `tenant` | string | Tenant identifier |
| `workspace` | string | Workspace name |
| `pipeline` | string | Pipeline name |
| `pipeline_run_id` | string | Run that produced this deadletter |
| `namespace` | string | Source namespace |
| `partition` | string | Partition value |
| `time_bucket` | bigint | Time bucket |
| `source_uri` | string | Source file/object |
| `offset_namespace` | string | Offset namespace |
| `offset_partition` | string | Offset partition |
| `offset_pos` | bigint | Offset position |
| `failure_code` | string | Error classification |
| `failure_error_messages` | array&lt;string&gt; | Error messages |
| `failure_error_kinds` | array&lt;string&gt; | Error kinds |
| `component` | string | Component that raised the error |
| `backtrace` | string | Stack trace (if available) |
| `record_raw_json` | string | Original raw record |
| `record_normalized_json` | string | Normalized record (if enabled) |
| `schema_hash` | string | Schema hash at time of failure |
| `schema_version` | int | Schema version |
| `metadata_snapshot` | string | Full metadata snapshot as JSON |
| `ingest_time_millis` | bigint | Ingestion timestamp |

## Querying deadletters with Athena

Create an external table over the deadletters prefix:

```sql
CREATE EXTERNAL TABLE IF NOT EXISTS deadletters (
  id string,
  tenant string,
  workspace string,
  pipeline string,
  pipeline_run_id string,
  namespace string,
  partition string,
  time_bucket bigint,
  source_uri string,
  offset_namespace string,
  offset_partition string,
  offset_pos bigint,
  failure_code string,
  failure_error_messages array<string>,
  failure_error_kinds array<string>,
  component string,
  backtrace string,
  record_raw_json string,
  record_normalized_json string,
  schema_hash string,
  schema_version int,
  metadata_snapshot string,
  ingest_time_millis bigint
)
PARTITIONED BY (namespace string, p_year string, p_month string, p_day string)
STORED AS PARQUET
LOCATION 's3://<your-state-bucket>/deadletters/<tenant>/<workspace>/<pipeline>/';
```

### Example queries

All deadletters for a namespace and date range:

```sql
SELECT id, namespace, failure_error_messages[1] AS err
FROM deadletters
WHERE namespace = 'bike_hire'
  AND p_year = '2025' AND p_month = '11';
```

Inspect the raw payload:

```sql
SELECT id,
       json_extract_scalar(record_raw_json, '$.device_id') AS device_id,
       failure_error_messages[1] AS err
FROM deadletters
WHERE namespace = 'bike_hire'
  AND p_year = '2025' AND p_month = '11' AND p_day = '06';
```

## Configuration

| Variable | Default | Description |
|---|---|---|
| `DEADLETTER_INCLUDE_NORMALIZED_JSON` | `yes` | Include the normalized JSON in deadletter records |
