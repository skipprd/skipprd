---
title: skipprd connect
description: Write skippr.yml from typed plugin configs. Same merge-writer as Python Config.save.
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

`--pipeline` and `--name` are required, and `connect` wires that entry into the pipeline. Existing `skippr.yml` or `skippr.yaml` is reused. A new file is created only when neither exists, and starts empty. The write reads the file, merges, and checks the result before it writes. If the check fails, nothing is written, including root flags such as `--workspace`:

- Each field you pass replaces that field. A flattened nested flag such as `--object-store-type` replaces the whole `object_store` block.
- Fields you did not pass, sibling pipelines, and other entries are kept.
- An entry cannot change plugin kind.
- `--store-name` requires `--store-type`.
- A Duckdb, SkipprLake, or AthenaIceberg schema sink and every data or deadletter sink of that plugin that links it hold one config. The fields you pass to `connect data-sink` or `connect schema-sink` go to all of them, and a new or empty one starts as a copy. If they already differ, the write is refused and nothing is overwritten.
- The check refuses plaintext secrets, two sinks on one namespace, a pair that differs, and a pipeline whose references do not resolve. An entry you are still filling in one field at a time is allowed until `discover`, `sync`, or a Session runs it. `skipprd doctor` reports the same findings.

Python `data_sink` and `deadletter_sink` registration pairs the same way. `Config.save` merges only and never rewrites a pair: it refuses a file whose pair differs.

Secrets must be exactly `${NAME}`; plaintext is rejected. Quote `${ENV}` in the shell so the shell does not expand it. Python secret fields take `skippr.EnvRef`:

```python
from skippr import Config, DataSinkPostgres, EnvRef, Pipeline

cfg = Config.discover()
warehouse = cfg.data_sink(
    "warehouse",
    DataSinkPostgres(
        host="localhost",
        user="skippr",
        password=EnvRef("POSTGRES_PASSWORD"),
        database="analytics",
    ),
)
src = cfg.get_data_source("sample")
cfg.pipeline("bikehire", Pipeline(data_source=src, data_sink=warehouse))
cfg.save()
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

Python `Config.save` uses the same merge-writer:

```python
from skippr import Config, DataSourceS3, LocalStorage, Pipeline

cfg = Config.discover().workspace("bikehire").storage(LocalStorage())
src = cfg.data_source("sample", DataSourceS3(s3_bucket="...", s3_prefix="..."))
cfg.pipeline("bikehire", Pipeline(data_source=src))
cfg.save()
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
