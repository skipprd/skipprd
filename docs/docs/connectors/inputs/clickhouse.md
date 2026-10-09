---
title: ClickHouse data source
description: Copy ClickHouse tables or the result of a SQL query into your destination with Skipprd over the ClickHouse HTTP interface.
---

# ClickHouse

The ClickHouse source reads tables, or the result of one SQL query, over the ClickHouse HTTP interface. It is a one-time full load: each table is copied once, and later sync runs do not load new or changed rows. Use it to move a ClickHouse dataset into another warehouse or lake.

## Before you begin

- **Network access.** The machine running Skipprd must reach the ClickHouse HTTP endpoint, usually port `8123` (HTTP) or `8443` (HTTPS).
- **A read-only user.** Skipprd only runs the `SELECT` statements you configure:

```sql
CREATE USER skippr IDENTIFIED BY 'change-me' SETTINGS readonly = 1;
GRANT SELECT ON analytics.* TO skippr;
```

## Configure

Store the password in an environment variable and reference it as `${CLICKHOUSE_PASSWORD}`. Skipprd also reads `.env` and `.env.local` next to `skippr.yml`.

```bash
export CLICKHOUSE_PASSWORD="change-me"
```

::: code-group

```python [Python]
from skippr import Config, DataSourceClickhouse, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "clickhouse",
    DataSourceClickhouse(
        url="https://clickhouse.internal:8443",
        database="analytics",
        user="skippr",
        password=EnvRef("CLICKHOUSE_PASSWORD"),
        tables=["events", "sessions"],
    ),
)
pipe = cfg.pipeline("clickhouse", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source clickhouse \
  --pipeline clickhouse \
  --name clickhouse \
  --url https://clickhouse.internal:8443 \
  --database analytics \
  --user skippr \
  --password '${CLICKHOUSE_PASSWORD}' \
  --tables events \
  --tables sessions
```

```yaml [YAML]
pipelines:
  clickhouse:
    data_source: data_sources.clickhouse

data_sources:
  clickhouse:
    Clickhouse:
      url: https://clickhouse.internal:8443
      database: analytics
      user: skippr
      password: ${CLICKHOUSE_PASSWORD}
      tables: ["events", "sessions"]
```

:::

To load the result of a query instead, set `query` and omit `tables`:

```yaml
data_sources:
  clickhouse:
    Clickhouse:
      url: https://clickhouse.internal:8443
      database: analytics
      user: skippr
      password: ${CLICKHOUSE_PASSWORD}
      query: "SELECT user_id, count() AS events FROM events GROUP BY user_id"
```

Add a destination to the pipeline (see the [connector catalog](/connectors/)), then discover the schema and run one sync pass:

::: code-group

```python [Python]
from skippr import Session

session = Session(pipe)
session.discover()
session.sync(once=True)
```

```bash [CLI]
skipprd discover --pipeline clickhouse --log
skipprd sync --pipeline clickhouse --once --log
```

:::

## Options

Set either `tables` or `query`. If you set both, Skipprd runs `query` and ignores `tables`.

| Key | Type | Default | Description |
|---|---|---|---|
| `url` | string | Required | ClickHouse HTTP endpoint, for example `http://localhost:8123` or `https://clickhouse.internal:8443`. |
| `database` | string | `default` | Database for unqualified table names. Also used in destination table names. |
| `user` | string | ClickHouse's default user | User to authenticate as. |
| `password` | `${ENV}` reference | none | Password for `user`. Must be an environment reference such as `${CLICKHOUSE_PASSWORD}`. |
| `tables` | list of strings | none | Tables to read. Each entry is used as written in `SELECT * FROM <entry>`. |
| `query` | string | none | One SQL `SELECT` to run instead of `tables`. Do not add a `FORMAT` clause; Skipprd adds `FORMAT JSONEachRow`. |
| `batch_size_rows` | integer | `10000` | Rows per batch handed to Skipprd. |
| `format` | string | `json` | Leave unset. Rows are always sent as JSON. |
| `batch_size_bytes` | integer | none | Not used by this source. |
| `batch_size_seconds` | integer | none | Not used by this source. |

## What gets synced

**Tables.** Each entry in `tables`, or the single result of `query`. Skipprd reads the whole result of each query before writing it, so size the machine for your largest table.

**Table names.** Each table lands in a destination table named `clickhouse_<database>_<table>`: `events` in `analytics` becomes `clickhouse_analytics_events`. A `query` lands in `clickhouse_<database>_query`. Skipprd lowercases names and replaces characters other than letters, digits, and `_` with `_`.

**Types.** ClickHouse renders each row as JSON (`JSONEachRow`), and Skipprd infers column types from those values when you run `skipprd discover`. See [Schema discovery and evolution](/concepts/schema).

**Incremental behaviour.** Skipprd records each table (or the query) as loaded once its rows are durably written. Later sync runs still send the query to ClickHouse, but its rows are not loaded again. To reload, run the source under a new pipeline name, which starts with fresh progress.

**Deletes.** Not captured.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `ClickHouse request failed` | Skipprd cannot reach `url`. | Check the scheme, host, and port, and that the endpoint is reachable from the machine running Skipprd. |
| `ClickHouse query error: ... Authentication failed` | Wrong user or password. | Check `user` and that `CLICKHOUSE_PASSWORD` is exported in the shell that runs Skipprd. |
| `ClickHouse query error: ... UNKNOWN_TABLE` | The table is not in `database`. | Fix `database`, or qualify the entry as `db.table`. |
| `ClickHouse query error: ... ACCESS_DENIED` | The user lacks `SELECT`. | Grant `SELECT` on the table or database. |
| `ClickHouse: must specify either 'tables' or 'query'` | Neither is set. | Add `tables` or `query`. |
| New rows never arrive | Each table loads once. | Run the source under a new pipeline name to take a fresh copy. |

## Next steps

- [Connector catalog](/connectors/) — pick a destination
- [ClickHouse destination](/connectors/outputs/clickhouse) — land data in ClickHouse
- [skipprd sync](/cli/sync) — run once or on a schedule
