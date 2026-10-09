---
title: PostgreSQL destination
description: Write Skipprd pipelines into PostgreSQL. Schemas and tables are created on the first sync.
---

# PostgreSQL

Use PostgreSQL when the destination is an application database or a small warehouse you already run. Skipprd creates the schema and tables, then inserts rows. It does not use logical replication on the write path.

## Before you begin

- The Skipprd host can reach `host:port` (default `5432`).
- A role that can `CREATE` on the target database (or an existing `schema` the role owns).
- Store the password in the environment. Do not put it in `skippr.yml`.

```bash
export POSTGRES_PASSWORD="change-me"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkPostgres, EnvRef, Pipeline

cfg = Config.discover()
warehouse = cfg.data_sink(
    "warehouse",
    DataSinkPostgres(
        host="localhost",
        port=5432,
        user="skippr",
        password=EnvRef("POSTGRES_PASSWORD"),
        database="analytics",
        schema="public",
        sslmode="prefer",
    ),
)
cfg.pipeline("files", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=warehouse))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink postgres \
  --pipeline files \
  --name warehouse \
  --host localhost \
  --port 5432 \
  --user skippr \
  --password '${POSTGRES_PASSWORD}' \
  --database analytics \
  --schema public \
  --sslmode prefer
```

```yaml [YAML]
data_sinks:
  warehouse:
    Postgres:
      host: localhost
      port: 5432
      user: skippr
      password: ${POSTGRES_PASSWORD}
      database: analytics
      schema: public
      sslmode: prefer
```

:::

```bash
skipprd discover --pipeline files
skipprd sync --pipeline files --once --log
```

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `host` | string | `localhost` | PostgreSQL host |
| `port` | integer | `5432` | Port |
| `user` | string | Required | Database user |
| `password` | secret | Not set | Password as `${ENV}` |
| `database` | string | Required | Database name |
| `schema` | string | `public` | Schema for tables |
| `sslmode` | string | Not set | libpq mode: `disable`, `prefer`, `require` |

## How data lands

On the first sync Skipprd runs `CREATE SCHEMA IF NOT EXISTS` and creates tables to match the discovered schema. New columns are added later. Each batch is inserted as rows. Retries are exactly once — see [Exactly-once delivery](/concepts/exactly-once).

Source namespaces become tables in `schema`. Prefer a dedicated schema so Skipprd tables stay out of the application schema.

## Troubleshooting

| Symptom | Fix |
|---|---|
| Connection refused | Check `host`, `port`, and that PostgreSQL accepts connections from this machine |
| Password authentication failed | Export the variable named in `password` in this shell |
| `permission denied for schema` | `GRANT CREATE` on the schema, or let Skipprd create one the role owns |
| SSL required / SSL off | Match `sslmode` to the server (`require` vs `disable`) |

## Next steps

- [PostgreSQL source](/connectors/inputs/postgres) — read from a different database
- [Quickstart: PostgreSQL](/getting-started/quickstart-postgres)
- [Exactly-once delivery](/concepts/exactly-once)
