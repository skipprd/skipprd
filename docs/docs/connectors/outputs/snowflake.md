---
title: Snowflake
description: Load Skipprd pipelines into Snowflake tables with key-pair authentication, automatic schema, and COPY INTO from a stage.
---

# Snowflake

Use Snowflake when that warehouse is where analysts already work. Skipprd creates the schema and tables, stages Parquet, and runs `COPY INTO`. You provide an account, a user, a warehouse, and a database.

Prefer a key pair. Password sign-in fails when the account enforces MFA.

## Before you begin

1. **Account identifier** in `orgname-accountname` form (Snowsight → account menu), for example `myorg-myaccount`.
2. **A role** with `USAGE` on the warehouse, database, and schema; `CREATE SCHEMA` on the database if Skipprd should create it; `CREATE TABLE` on the schema; `INSERT` and `SELECT` on the tables.
3. **A key pair** (recommended). Create an unencrypted PKCS#8 key and attach the public key to the user. See the [Snowflake quickstart](/getting-started/quickstart-snowflake) for the SQL.
4. **A stage** Skipprd can `PUT` to. The user stage `@~` works. A named stage such as `@SKIPPR_STAGE` is fine.

```bash
export SNOWFLAKE_PRIVATE_KEY_PATH="/path/to/rsa_key.p8"
```

## Configure

These examples assume a source named `sample` already exists.

::: code-group

```python [Python]
from skippr import Config, DataSinkSnowflake, Pipeline

cfg = Config.discover()
warehouse = cfg.data_sink(
    "warehouse",
    DataSinkSnowflake(
        account="myorg-myaccount",
        user="skippr_loader",
        private_key_path="${SNOWFLAKE_PRIVATE_KEY_PATH}",
        warehouse="COMPUTE_WH",
        database="RAW_DATA",
        schema="PUBLIC",
        role="LOADER_ROLE",
        stage="@SKIPPR_STAGE",
    ),
)
cfg.pipeline("bikehire", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=warehouse))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink snowflake \
  --pipeline bikehire \
  --name warehouse \
  --account myorg-myaccount \
  --user skippr_loader \
  --private-key-path '${SNOWFLAKE_PRIVATE_KEY_PATH}' \
  --warehouse COMPUTE_WH \
  --database RAW_DATA \
  --schema PUBLIC \
  --role LOADER_ROLE \
  --stage '@SKIPPR_STAGE'
```

```yaml [YAML]
data_sinks:
  warehouse:
    Snowflake:
      account: myorg-myaccount
      user: skippr_loader
      private_key_path: ${SNOWFLAKE_PRIVATE_KEY_PATH}
      warehouse: COMPUTE_WH
      database: RAW_DATA
      schema: PUBLIC
      role: LOADER_ROLE
      stage: "@SKIPPR_STAGE"
```

:::

```bash
skipprd discover --pipeline bikehire
skipprd sync --pipeline bikehire --once --log
```

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `account` | string | Required | Account identifier (`myorg-myaccount` or `xy12345.us-east-1`) |
| `user` | string | Required | Snowflake user |
| `warehouse` | string | Required | Compute warehouse |
| `database` | string | Required | Target database |
| `schema` | string | Required | Target schema |
| `private_key_path` | path | Not set | PKCS#8 PEM for key-pair sign-in. Preferred when set. |
| `password` | secret | Not set | Password sign-in. Ignored when `private_key_path` is set. Use `${ENV}`. |
| `role` | string | Not set | Role to assume after sign-in |
| `stage` | string | `@~` | Stage for uploads. Named stages such as `@SKIPPR_STAGE` work. |
| `staging_uri` | URI | Not set | Upload to this `s3://`, `azure://`, or `gcs://` prefix instead of the Snowflake stage |
| `staging_storage_integration` | string | Not set | Snowflake storage integration. Required for GCS staging. |
| `staging_azure_sas_token` | secret | Not set | SAS token when `staging_uri` is Azure |
| `staging_azure_account_key` | secret | Not set | Account key when `staging_uri` is Azure |
| `staging_gcs_service_account_key_path` | path | Not set | Service-account JSON when `staging_uri` is GCS |
| `max_concurrency` | integer | Not set | Query/model only; ingest ignores it |
| `discovery_cache_ttl_secs` | integer | Not set | Query/model only; ingest ignores it |

## How data lands

Skipprd creates the schema and tables on the first sync, then adds columns when the source schema grows.

Each source namespace becomes one table: dots become underscores and the name is lowercased (`s3.events.click_stream` → `s3_events_click_stream`).

Each batch is staged as Parquet and loaded with `COPY INTO` (`MATCH_BY_COLUMN_NAME = CASE_INSENSITIVE`). The staged file is removed after a successful load. Retries do not duplicate rows — see [Exactly-once delivery](/concepts/exactly-once).

| Skipprd type | Snowflake type |
|---|---|
| String | `VARCHAR` |
| Integer / Long | `NUMBER(38,0)` |
| Double | `DOUBLE` |
| Boolean | `BOOLEAN` |
| Date | `DATE` |
| Timestamp | `TIMESTAMP_NTZ` |
| Struct | `OBJECT(...)` |
| Array | `ARRAY(...)` |
| Map | `MAP(...)` |

Optional `staging_uri` writes Parquet to your own bucket instead of the Snowflake stage. GCS staging needs `staging_storage_integration` because Snowflake will not take inline GCS credentials on `COPY INTO`.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `Failed to connect: 250001` | Check `account` — use `ORG-ACCOUNT` or include the region |
| `Incorrect username or password` | Confirm `user` and that `private_key_path` or `password` is exported in this shell |
| `Insufficient privileges` | Grant the privileges listed above to `role` |
| `390197` MFA required | Switch to key-pair auth |
| `openssl: command not found` | Install OpenSSL and recreate the key |

## Next steps

- [Quickstart: Snowflake](/getting-started/quickstart-snowflake)
- [Exactly-once delivery](/concepts/exactly-once)
- [skipprd sync](/cli/sync)
