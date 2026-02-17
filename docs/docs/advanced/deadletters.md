## Deadletters: Direct S3 uploads and SQL queries

Deadletters are emitted directly to your Skippr state bucket with no WAL/buffers/compaction.

- Bucket: `SKIPPR_S3_BUCKET` / `skippr.skippr_s3_bucket`
- Key layout (Hive-style partitions, multi-tenant): `deadletters/<tenant>/<workspace>/<pipeline>/namespace=<namespace>/p_year=<YYYY>/p_month=<MM>/p_day=<DD>/<id>.parquet`
- Each object is a single JSON document containing the full event with a full metadata snapshot for the namespace.
- A concise stdout log line is printed on emission: `Deadletter id=<id> ns=<namespace> err=<first_error>`

### Storage format

- Files are written as Parquet with a single row per failure event. Columns include identifiers (tenant, workspace, pipeline, pipeline_run_id, id), offsets, taxonomy (`failure_code`, `failure_error_messages`), raw and normalized JSON, full metadata snapshot (as JSON string), and timestamps. 

### Athena external table (Parquet)

You can register an external table over the `deadletters/` prefix. Below example partitions by day (`dt`); the `namespace` is included as a JSON column, so you can filter by `namespace='bike_hire'` directly.

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

If you prefer, you can use partition projection for `dt` to avoid MSCK.

### Queries

- All deadletters for a namespace and day range:

```sql
SELECT id, namespace, failure.error_messages[1] AS err
FROM deadletters
WHERE namespace = 'bike_hire'
  AND dt BETWEEN '2025-11-05' AND '2025-11-07';
```

- Inspect fields within the raw payload:

```sql
SELECT id,
       json_extract_scalar(record.raw_json, '$.device_id') AS device_id,
       failure.error_messages[1] AS err
FROM deadletters
WHERE namespace = 'bike_hire'
  AND dt = '2025-11-06';
```

- Filter by component/kind:

```sql
SELECT *
FROM deadletters
WHERE namespace = 'bike_hire'
  AND failure.component = 'ingest';
```

Notes:
- `normalized_json` is included by default (`DEADLETTER_INCLUDE_NORMALIZED_JSON=yes`).
- Objects are written immediately on failure; there is no WAL or background compaction step.

