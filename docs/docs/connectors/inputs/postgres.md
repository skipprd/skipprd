---
title: PostgreSQL data source
description: Copy PostgreSQL tables into your destination with Skipprd, once as a snapshot or continuously with logical-replication change data capture.
---

# PostgreSQL

The PostgreSQL source reads tables from one PostgreSQL database. Choose how it reads with `cdc_mode`:

- **`snapshot`** (default) reads each table in full once. Later sync runs skip tables that are already loaded, so new and changed rows are not picked up. Use it for one-off migrations and backfills.
- **`snapshot_then_cdc`** copies the tables, then streams inserts, updates, and deletes from PostgreSQL logical replication. Each run resumes from the last committed transaction. Use it to keep a destination in step with a live database.
- **`cdc_only`** streams changes from now on, without the initial copy.

## Before you begin

- **Network access.** The machine running Skipprd must reach the server on its port (default `5432`).
- **No TLS.** Skipprd connects to PostgreSQL without encryption. The server must accept non-SSL connections from Skipprd's address (a `host` line in `pg_hba.conf`, not only `hostssl`). Run Skipprd on a private network or through a tunnel. Servers that force SSL, such as Amazon RDS with `rds.force_ssl = 1`, refuse the connection.
- **A read-only role.** Skipprd only runs `SELECT` statements against your tables:

```sql
CREATE ROLE skippr_reader WITH LOGIN PASSWORD 'change-me';
GRANT CONNECT ON DATABASE app TO skippr_reader;
GRANT USAGE ON SCHEMA public TO skippr_reader;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO skippr_reader;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO skippr_reader;
```

Repeat the `USAGE` and `SELECT` grants for every other schema you list in `tables`.

### Extra setup for CDC

Skip this if you use `snapshot`.

1. Set `wal_level = logical` and restart PostgreSQL. Check it with `SHOW wal_level;`.
2. Give the role replication rights:

   ```sql
   ALTER ROLE skippr_reader WITH REPLICATION;
   -- Amazon RDS / Aurora instead: GRANT rds_replication TO skippr_reader;
   ```

3. Create the publication yourself. If `skippr_publication` (or your `publication_name`) does not exist, Skipprd runs `CREATE PUBLICATION ... FOR ALL TABLES`, which needs a superuser. Creating it as an admin keeps the Skipprd role unprivileged and lets you choose the tables:

   ```sql
   CREATE PUBLICATION skippr_publication FOR TABLE public.customers, public.orders;
   ```

4. Give every published table a primary key or a replica identity. PostgreSQL rejects `UPDATE` and `DELETE` on a published table that has neither:

   ```sql
   ALTER TABLE public.events REPLICA IDENTITY FULL;
   ```

On its first CDC run Skipprd creates a logical replication slot named `skippr_slot` (or your `replication_slot_name`). The slot holds back PostgreSQL's write-ahead log until Skipprd reads it, so a stopped pipeline makes the server's disk grow. When you retire the pipeline, drop the slot:

```sql
SELECT pg_drop_replication_slot('skippr_slot');
```

## Configure

Store the password in an environment variable, then reference it as `${POSTGRES_PASSWORD}`. Skipprd also reads `.env` and `.env.local` next to `skippr.yml`.

```bash
export POSTGRES_PASSWORD="change-me"
```

This example copies two tables once:

::: code-group

```python [Python]
from skippr import Config, DataSourcePostgres, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "app_db",
    DataSourcePostgres(
        host="db.internal",
        port=5432,
        user="skippr_reader",
        password=EnvRef("POSTGRES_PASSWORD"),
        database="app",
        tables=["customers", "orders"],
    ),
)
pipe = cfg.pipeline("app_db", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source postgres \
  --pipeline app_db \
  --name app_db \
  --host db.internal \
  --port 5432 \
  --user skippr_reader \
  --password '${POSTGRES_PASSWORD}' \
  --database app \
  --tables customers \
  --tables orders
```

```yaml [YAML]
pipelines:
  app_db:
    data_source: data_sources.app_db

data_sources:
  app_db:
    Postgres:
      host: db.internal
      port: 5432
      user: skippr_reader
      password: ${POSTGRES_PASSWORD}
      database: app
      tables: ["customers", "orders"]
```

:::

To stream changes after the copy, add `cdc_mode`. Set `host`, `port`, `user`, `password`, and `database` individually: the replication stream does not read `connection_string`.

::: code-group

```python [Python]
from skippr import Config, DataSourcePostgres, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "app_db",
    DataSourcePostgres(
        host="db.internal",
        user="skippr_reader",
        password=EnvRef("POSTGRES_PASSWORD"),
        database="app",
        cdc_mode="snapshot_then_cdc",
    ),
)
pipe = cfg.pipeline("app_db", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source postgres \
  --pipeline app_db \
  --name app_db \
  --cdc-mode snapshot_then_cdc
```

```yaml [YAML]
data_sources:
  app_db:
    Postgres:
      host: db.internal
      user: skippr_reader
      password: ${POSTGRES_PASSWORD}
      database: app
      cdc_mode: snapshot_then_cdc
```

:::

Add a destination to the pipeline (see the [connector catalog](/connectors/)), then discover the schema and run one sync pass:

::: code-group

```python [Python]
from skippr import Session

session = Session(pipe)
session.discover()
session.sync(once=True)
```

```bash [CLI]
skipprd discover --pipeline app_db --log
skipprd sync --pipeline app_db --once --log
```

:::

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `host` | string | `localhost` | Server hostname or IP address. |
| `port` | integer | `5432` | Server port. |
| `user` | string | `postgres` | Role to connect as. |
| `password` | `${ENV}` reference | empty | Password for `user`. Must be an environment reference such as `${POSTGRES_PASSWORD}`. |
| `database` | string | none | Database to read. Set it unless you use `connection_string`. |
| `connection_string` | `${ENV}` reference | none | Full connection string, either `host=... port=... user=... password=... dbname=...` or a `postgresql://user:pass@host:5432/db` URL. Replaces the individual fields for snapshot reads only; CDC always uses them. |
| `tables` | list of strings | every table in `public` | Tables to read. Each entry is used as written in `SELECT * FROM <entry>`, so schema-qualify (`sales.orders`) and double-quote mixed-case names (`'"Orders"'`). Views work in snapshot reads. |
| `cdc_mode` | `snapshot`, `snapshot_then_cdc`, or `cdc_only` | `snapshot` | How to read. See the introduction above. |
| `replication_slot_name` | string | `skippr_slot` | Logical replication slot for CDC. Use a different slot for each pipeline that reads the same database. |
| `publication_name` | string | `skippr_publication` | Publication that CDC subscribes to. Created `FOR ALL TABLES` if it does not exist. |
| `cdc_idle_timeout_seconds` | integer | unset | End the change stream after this many seconds without a change. Unset or `0` keeps streaming; with `skipprd sync --once`, Skipprd ends the run once the stream goes idle. |
| `batch_size_rows` | integer | `10000` | Rows per batch handed to Skipprd. |
| `query` | string | none | Not supported by this source and ignored. List tables or views in `tables` instead. |
| `format` | string | `json` | Leave unset. Rows are always sent as JSON. |
| `batch_size_bytes` | integer | none | Not used by this source. |
| `batch_size_seconds` | integer | none | Not used by this source. |

## What gets synced

**Tables.** The tables in `tables`, or every table in the `public` schema when you omit it. Skipprd reads each table with one `SELECT *` and holds the result in memory before writing it, so size the machine for your largest table.

**Table names.** Each source table lands in a destination table named `postgres_<table>`: `orders` becomes `postgres_orders` and `sales.orders` becomes `postgres_sales_orders`. Skipprd lowercases names and replaces any character other than letters, digits, and `_` with `_`.

**Types in snapshot reads.**

| PostgreSQL type | Arrives as |
|---|---|
| `text`, `varchar`, `char`, `name`, `citext` | string |
| `bigint`, `integer` | number |
| `double precision` | number |
| `boolean` | boolean |
| Any other type, such as `smallint`, `real`, `numeric`, `date`, `timestamp`, `uuid`, `json`, `jsonb`, `bytea`, arrays | `null` |

To keep those columns in a snapshot, point `tables` at a view that casts them to text, for example `CREATE VIEW skippr_orders AS SELECT id, amount::text AS amount, created_at::text AS created_at FROM orders;`.

**Types in change events.** CDC delivers every column value as a string in PostgreSQL's text format (`"42"`, `"2024-05-01 09:30:00+00"`), and SQL `NULL` as `null`. Run `skipprd discover` to infer column types; see [Schema discovery and evolution](/concepts/schema).

**Incremental behaviour.** In `snapshot` mode, Skipprd records each table as loaded once its rows are durably written; later runs skip it. To reload, run the source under a new pipeline name, which starts with fresh progress. In CDC modes, the snapshot is anchored to the replication slot's position, and changes made during the copy arrive through the stream. Skipprd saves its position after each committed transaction and resumes from there.

**What CDC captures.** Every table in the publication, including tables not listed in `tables`; `tables` only limits the initial snapshot. Inserts and updates carry the new row. Deletes carry the old row as PostgreSQL publishes it: only the key columns, unless the table uses `REPLICA IDENTITY FULL`. Truncates are not captured.

**Deletes.** Only CDC modes capture deletes. Whether a delete removes the destination row depends on the destination and your pipeline's CDC settings; see [Change data capture](/cdc/) and [CDC guarantees](/cdc/guarantees).

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `connection refused` or a timeout | Skipprd cannot reach the server. | Check `host`, `port`, firewalls, and security groups from the machine running Skipprd. |
| `password authentication failed` | Wrong role or password. | Check `user` and that `POSTGRES_PASSWORD` is exported in the shell that runs Skipprd. |
| `no pg_hba.conf entry ... SSL off`, or another SSL error | The server only accepts encrypted connections. | Allow a non-SSL `host` entry for Skipprd's address on a private network. Skipprd cannot connect with TLS. |
| `relation "..." does not exist` | The table is outside `public`, or its name has capitals. | Schema-qualify the entry in `tables` and double-quote mixed-case names. |
| `permission denied for table ...` | The role lacks `SELECT`. | Run the grants in [Before you begin](#before-you-begin) for that schema. |
| A column is `null` in every row | Its type is not read in snapshot mode. | Cast it to text in a view and list the view in `tables`. |
| New rows never arrive | `snapshot` mode loads each table once. | Switch to `snapshot_then_cdc`, or use a new pipeline name to reload. |
| `creating Postgres publication ... failed` | `FOR ALL TABLES` needs a superuser. | Create the publication as an admin, as shown above. |
| `creating Postgres logical replication slot ... failed` | `wal_level` is not `logical`, or the role lacks `REPLICATION`. | Complete [Extra setup for CDC](#extra-setup-for-cdc). |
| `replication slot "skippr_slot" is active` | Another pipeline or client is using the slot. | Give each pipeline its own `replication_slot_name`. |
| `cannot update table ... because it does not have a replica identity` (in your application) | A published table has no primary key. | Add a primary key or set `REPLICA IDENTITY FULL` on that table. |
| The database server's disk keeps growing | A CDC slot is retaining log for a stopped pipeline. | Restart the pipeline, or drop the slot if you have retired it. |

## Next steps

- [Change data capture](/cdc/) — how CDC changes are applied in the destination
- [PostgreSQL destination](/connectors/outputs/postgres) — land data in PostgreSQL
- [skipprd sync](/cli/sync) — run once or on a schedule
