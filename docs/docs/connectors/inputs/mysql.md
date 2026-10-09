---
title: MySQL data source
description: Copy MySQL tables into your destination with Skipprd, once as a snapshot or continuously with binlog change data capture.
---

# MySQL

The MySQL source reads tables from a MySQL server. Choose how it reads with `cdc_mode`:

- **`snapshot`** (default) reads each table in full once. Later sync runs skip tables that are already loaded, so new and changed rows are not picked up. Use it for one-off migrations and backfills.
- **`snapshot_then_cdc`** copies the tables, then streams inserts, updates, and deletes from the binary log (binlog). Each run resumes from the last saved binlog position. Use it to keep a destination in step with a live database.
- **`cdc_only`** streams changes from the current binlog position, without the initial copy.

## Before you begin

- **Network access.** The machine running Skipprd must reach the server on its port (default `3306`). The connection is unencrypted unless you add `?require_ssl=true` to the connection string.
- **A read-only user.** Grant `SELECT` only on the databases you want to load. When you omit `tables`, Skipprd loads every base table the user can see, in every database, so narrow grants keep it to the data you intend:

```sql
CREATE USER 'skippr'@'%' IDENTIFIED BY 'change-me';
GRANT SELECT ON shop.* TO 'skippr'@'%';
```

### Extra setup for CDC

Skip this if you use `snapshot`.

1. Turn on row-based binary logging with full row images. In `my.cnf` (or your managed service's parameter group):

   ```ini
   log_bin = mysql-bin
   binlog_format = ROW
   binlog_row_image = FULL
   ```

   Skipprd maps binlog values to columns by position, so it needs the full row image.

2. Grant replication privileges:

   ```sql
   GRANT REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'skippr'@'%';
   ```

3. Choose a `server_id` that no other MySQL server or replica in the topology uses. Skipprd joins the server as a replica with that ID. The default, `1`, is often the primary's own ID.
4. Keep binlogs long enough to cover your longest pause. If Skipprd is stopped for longer than `binlog_expire_logs_seconds`, the saved position is purged and the stream cannot resume.

## Configure

The connection string holds the password, so keep the whole string in an environment variable. Skipprd also reads `.env` and `.env.local` next to `skippr.yml`.

```bash
export MYSQL_CONNECTION_STRING="mysql://skippr:change-me@db.internal:3306/shop"
```

This example copies two tables once:

::: code-group

```python [Python]
from skippr import Config, DataSourceMysql, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "shop_db",
    DataSourceMysql(
        connection_string=EnvRef("MYSQL_CONNECTION_STRING"),
        tables=["shop.customers", "shop.orders"],
    ),
)
pipe = cfg.pipeline("shop_db", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source mysql \
  --pipeline shop_db \
  --name shop_db \
  --connection-string '${MYSQL_CONNECTION_STRING}' \
  --tables shop.customers \
  --tables shop.orders
```

```yaml [YAML]
pipelines:
  shop_db:
    data_source: data_sources.shop_db

data_sources:
  shop_db:
    Mysql:
      connection_string: ${MYSQL_CONNECTION_STRING}
      tables: ["shop.customers", "shop.orders"]
```

:::

To stream changes after the copy:

::: code-group

```python [Python]
from skippr import Config, DataSourceMysql, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "shop_db",
    DataSourceMysql(
        connection_string=EnvRef("MYSQL_CONNECTION_STRING"),
        tables=["shop.customers", "shop.orders"],
        cdc_mode="snapshot_then_cdc",
        server_id=4201,
    ),
)
pipe = cfg.pipeline("shop_db", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source mysql \
  --pipeline shop_db \
  --name shop_db \
  --cdc-mode snapshot_then_cdc \
  --server-id 4201
```

```yaml [YAML]
data_sources:
  shop_db:
    Mysql:
      connection_string: ${MYSQL_CONNECTION_STRING}
      tables: ["shop.customers", "shop.orders"]
      cdc_mode: snapshot_then_cdc
      server_id: 4201
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
skipprd discover --pipeline shop_db --log
skipprd sync --pipeline shop_db --once --log
```

:::

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `connection_string` | `${ENV}` reference | Required | Connection URL, `mysql://user:password@host:3306/database`. The database in the URL is the default for unqualified table names. Add `?require_ssl=true` to encrypt the connection. |
| `tables` | list of strings | every base table the user can see | Tables to read, as `database.table` or `table` (resolved in the URL's database). |
| `cdc_mode` | `snapshot`, `snapshot_then_cdc`, or `cdc_only` | `snapshot` | How to read. See the introduction above. |
| `server_id` | integer | `1` | Replica ID Skipprd uses when it reads the binlog. Must be unique in your replication topology. |
| `cdc_idle_timeout_seconds` | integer | unset | End the binlog stream after this many seconds without an event. Unset or `0` keeps streaming; with `skipprd sync --once`, Skipprd ends the run once the stream goes idle. |
| `format` | string | `json` | Leave unset. Rows are always sent as JSON. |
| `batch_size_bytes` | integer | none | Not used by this source. Rows are sent in batches of 10,000. |
| `batch_size_seconds` | integer | none | Not used by this source. |

## What gets synced

**Tables.** The tables in `tables`, or every base table visible to the user when you omit it. Skipprd reads each table with one `SELECT *` and holds the result in memory before writing it, so size the machine for your largest table.

**Table names.** Each source table lands in a destination table named after the table alone: `shop.customers` becomes `customers`. Skipprd lowercases names and replaces characters other than letters, digits, and `_` with `_`. Tables with the same name in different databases land in the same destination table, so list tables from one database per pipeline.

**Types.** Snapshot reads use MySQL's text protocol, so every value arrives as a string, for example `"42"` or `"2024-05-01 09:30:00"`. Change events carry typed values: integers and floating-point numbers as numbers, and dates and times as strings like `"2024-05-01 09:30:00.000000"`. Binary values are read as UTF-8 text, so bytes that are not valid UTF-8 are not preserved exactly. Run `skipprd discover` to infer column types; see [Schema discovery and evolution](/concepts/schema).

**Incremental behaviour.** In `snapshot` mode, Skipprd records each table as loaded once its rows are durably written; later runs skip it. To reload, run the source under a new pipeline name, which starts with fresh progress. In CDC modes, Skipprd captures the binlog position before the copy, so changes made during the copy arrive through the stream. It saves its position after each event and resumes from there.

**What CDC captures.** Row inserts, updates, and deletes for every table written to the binlog, not only the tables in `tables`; `tables` only limits the initial snapshot. Skipprd loads column names when each sync run starts. After an `ALTER TABLE`, restart the sync so new columns are named correctly; until then they arrive as `col_<position>`.

**Deletes.** Only CDC modes capture deletes. A delete carries the row as it was before deletion. Whether it removes the destination row depends on the destination and your pipeline's CDC settings; see [Change data capture](/cdc/) and [CDC guarantees](/cdc/guarantees).

## Troubleshooting

Run sync with `--log` to see these messages. In snapshot mode a connection failure is logged, and the run ends without loading anything.

| Symptom | Cause | Fix |
|---|---|---|
| `Failed to connect to MySQL` in the log | Wrong host, port, credentials, or network path. | Check the connection string and that the server is reachable from the machine running Skipprd. |
| `Failed to query table ...` in the log, and that table is missing | The table name is wrong or the user lacks `SELECT`. | Fix the entry in `tables`, or grant `SELECT` on it. Other tables still load. |
| Unexpected system or other databases load | `tables` is omitted and the user can see more than you intended. | List `tables`, or narrow the user's grants. |
| New rows never arrive | `snapshot` mode loads each table once. | Switch to `snapshot_then_cdc`, or use a new pipeline name to reload. |
| `is binary logging enabled?` | `log_bin` is off. | Enable binary logging as shown in [Extra setup for CDC](#extra-setup-for-cdc). |
| `Binlog stream open: ... Access denied` | The user lacks replication privileges. | Grant `REPLICATION SLAVE, REPLICATION CLIENT`. |
| Binlog stream drops or another replica disconnects | Two replicas share a `server_id`. | Set a unique `server_id`. |
| Stream cannot resume after a long pause | The saved binlog file was purged. | Increase binlog retention, then start a new pipeline to take a fresh snapshot. |
| Columns named `col_0`, `col_1`, … | The table changed shape during the stream, or the user cannot see its columns. | Restart the sync, and check the user has `SELECT` on the table. |

## Next steps

- [Change data capture](/cdc/) — how CDC changes are applied in the destination
- [Connector catalog](/connectors/) — pick a destination
- [skipprd sync](/cli/sync) — run once or on a schedule
