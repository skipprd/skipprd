---
title: MotherDuck data source
description: Copy MotherDuck tables or the result of a SQL query into your destination with Skipprd using a MotherDuck access token.
---

# MotherDuck

The MotherDuck source reads tables, or the result of one SQL query, from MotherDuck over HTTPS with your access token. It is a one-time full load: each table is copied once, and later sync runs do not load new or changed rows. Use it to move MotherDuck data into another warehouse or lake.

## Before you begin

- **Network access.** The machine running Skipprd must reach `https://api.motherduck.com` over HTTPS.
- **An access token.** Create a MotherDuck access token for a user that can read the database. Skipprd only runs the `SELECT` statements you configure.

## Configure

Store the token in an environment variable and reference it as `${MOTHERDUCK_TOKEN}`. Skipprd also reads `.env` and `.env.local` next to `skippr.yml`.

```bash
export MOTHERDUCK_TOKEN="your-access-token"
```

::: code-group

```python [Python]
from skippr import Config, DataSourceMotherduck, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "motherduck",
    DataSourceMotherduck(
        motherduck_token=EnvRef("MOTHERDUCK_TOKEN"),
        database="analytics",
        tables=["users", "events"],
    ),
)
pipe = cfg.pipeline("motherduck", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source motherduck \
  --pipeline motherduck \
  --name motherduck \
  --motherduck-token '${MOTHERDUCK_TOKEN}' \
  --database analytics \
  --tables users \
  --tables events
```

```yaml [YAML]
pipelines:
  motherduck:
    data_source: data_sources.motherduck

data_sources:
  motherduck:
    Motherduck:
      motherduck_token: ${MOTHERDUCK_TOKEN}
      database: analytics
      tables: ["users", "events"]
```

:::

To load the result of a query instead, set `query` and omit `tables`:

```yaml
data_sources:
  motherduck:
    Motherduck:
      motherduck_token: ${MOTHERDUCK_TOKEN}
      database: analytics
      query: "SELECT country, count(*) AS users FROM users GROUP BY country"
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
skipprd discover --pipeline motherduck --log
skipprd sync --pipeline motherduck --once --log
```

:::

## Options

Set either `tables` or `query`. If you set both, Skipprd runs `query` and ignores `tables`.

| Key | Type | Default | Description |
|---|---|---|---|
| `motherduck_token` | `${ENV}` reference | Required | MotherDuck access token. Must be an environment reference such as `${MOTHERDUCK_TOKEN}`. |
| `database` | string | none | Database to run queries in. Also used in destination table names. |
| `tables` | list of strings | none | Tables to read. Each entry is used as written in `SELECT * FROM <entry>`. |
| `query` | string | none | One SQL `SELECT` to run instead of `tables`. |
| `batch_size_rows` | integer | `10000` | Rows per batch handed to Skipprd. |
| `format` | string | `json` | Leave unset. Rows are always sent as JSON. |
| `batch_size_bytes` | integer | none | Not used by this source. |
| `batch_size_seconds` | integer | none | Not used by this source. |

## What gets synced

**Tables.** Each entry in `tables`, or the single result of `query`. Skipprd reads the whole result of each query before writing it.

**Table names.** Each table lands in a destination table named `motherduck_<database>_<table>`: `users` in `analytics` becomes `motherduck_analytics_users`. Without `database`, the middle part is `motherduck`. A `query` lands in `motherduck_<database>_query`. Skipprd lowercases names and replaces characters other than letters, digits, and `_` with `_`.

**Types.** Skipprd infers column types from the returned JSON values when you run `skipprd discover`. See [Schema discovery and evolution](/concepts/schema).

**Incremental behaviour.** Skipprd records each table (or the query) as loaded once its rows are durably written. Later sync runs still send the query to MotherDuck, but its rows are not loaded again. To reload, run the source under a new pipeline name, which starts with fresh progress.

**Deletes.** Not captured.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `MotherDuck API HTTP 401` or `403` | The token is wrong, expired, or not exported. | Check that `MOTHERDUCK_TOKEN` is exported in the shell that runs Skipprd and that the token is still valid. |
| `MotherDuck API HTTP 4xx` naming a missing table or catalog | `database` or a table name is wrong. | Fix `database` or the entry in `tables`. |
| `MotherDuck request: ...` | Skipprd cannot reach the MotherDuck API. | Allow outbound HTTPS to `api.motherduck.com`. |
| `MotherDuck: must specify either 'tables' or 'query'` | Neither is set. | Add `tables` or `query`. |
| New rows never arrive | Each table loads once. | Run the source under a new pipeline name to take a fresh copy. |

## Next steps

- [Connector catalog](/connectors/) — pick a destination
- [MotherDuck destination](/connectors/outputs/motherduck) — land data in MotherDuck
- [skipprd sync](/cli/sync) — run once or on a schedule
