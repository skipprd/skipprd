---
title: skipprd connect
description: Write skippr.yml from typed plugin configs. Same engine as Python Session.connect.
---

# skipprd connect

`skipprd connect` writes `skippr.yml`. It reflects in-tree plugin config structs. There is no second field catalog. Secrets persist as `${ENV}` references, never plaintext.

Root `skippr:` keys are global flags, not a `connect skippr` verb:

```bash
skipprd --workspace bikehire --storage-mode local connect data-source s3 \
  --pipeline bikehire \
  --name sample \
  --s3-bucket skippr-public-sample-data \
  --s3-prefix bike-hire
```

`--storage-mode` is `local` or `s3`. `--store-type` is `sled`, `dynamodb`, or `cloud-tables`. `--store-name` is the SkipprStore table. WAL backend stays `--wal-storage` / `WAL_STORAGE` (not a YAML key).

## Roles

| Command | Registry |
|---|---|
| `connect data-source` | `data_sources` |
| `connect data-sink` | `data_sinks` |
| `connect schema-sink` | `schema_sinks` |

`--pipeline` and `--name` are required. Existing `skippr.yml` or `skippr.yaml` is reused. A new file is created only when neither exists. The write is read → parse → merge → write, so sibling pipelines and extra keys on the same plugin survive.

Plaintext secrets are rejected. Python takes `${ENV}` as a string. Quote it in the shell so the shell does not expand it.

```python
import skippr
from skippr import DataSink

s = skippr.Session(pipeline="bikehire")
s.connect().data_sink(
    DataSink.Postgres,
    skippr.DataSinkPostgres(
        host="localhost",
        user="skippr",
        password="${POSTGRES_PASSWORD}",
        database="analytics",
    ),
).name("warehouse")
```

```bash
skipprd connect data-sink postgres \
  --pipeline bikehire \
  --name warehouse \
  --host localhost \
  --user skippr \
  --password '${POSTGRES_PASSWORD}' \
  --database analytics
```

```bash
skipprd connect data-sink snowflake \
  --pipeline bikehire \
  --name warehouse \
  --account '${SNOWFLAKE_ACCOUNT}' \
  --user '${SNOWFLAKE_USER}' \
  --password '${SNOWFLAKE_PASSWORD}' \
  --warehouse COMPUTE_WH \
  --database ANALYTICS \
  --schema BRONZE
```

Python uses the same persist path:

```python
import skippr
from skippr import DataSource, StorageMode

(
    skippr.workspace("bikehire")
    .storage_mode(StorageMode.LOCAL)
)

s = skippr.Session(pipeline="bikehire")
s.connect().data_source(
    DataSource.S3,
    skippr.DataSourceS3(s3_bucket="...", s3_prefix="..."),
).name("sample")
```

See [Python](/python).

SkipprLake R2 credentials are nested `object_store`. CLI flatten uses `--object-store-type r2` plus endpoint and `${OBJECTS_*}` secrets:

```bash
skipprd connect data-sink skippr-lake \
  --pipeline bikehire \
  --name lake \
  --catalog-table my-iceberg-catalog \
  --warehouse 's3://my-iceberg-warehouse/' \
  --object-store-type r2 \
  --object-store-endpoint '${OBJECTS_S3_ENDPOINT}' \
  --object-store-access-key-id '${OBJECTS_ACCESS_KEY_ID}' \
  --object-store-secret-access-key '${OBJECTS_SECRET_ACCESS_KEY}'
```
