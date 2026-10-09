---
title: CLI overview
description: Check your skipprd install, learn the global flags, and find the right command for each step from connect to query.
---

# CLI overview

`skipprd` is the Skipprd command-line tool. You use it to build a `skippr.yml`, discover the shape of your source data, sync it to a destination, and query the result. Every command reads the same `skippr.yml`, and the Python `skippr.Session` runs the same engine, so you can mix the CLI and Python freely.

## Check your install

```bash
skipprd --version
```

You see the installed version, for example:

```text
skipprd 17.0.0
```

If the command is not found, follow [Install](/getting-started/install).

## Commands

| Command | Use it to |
|---|---|
| [`skipprd connect`](/cli/connect) | Add a source, destination, or schema sink to `skippr.yml` without hand-editing YAML. |
| [`skipprd discover`](/cli/discover) | Read the source and infer the schema of each namespace. Never writes to the destination. |
| [`skipprd schema`](/cli/schema) | Print the columns of Parquet files a pipeline wrote to its local data directory. |
| [`skipprd metadata`](/cli/metadata) | Show the schema `discover` saved, or apply a reviewed schema for one namespace. |
| [`skipprd sync`](/cli/sync) | Ingest from the source, buffer through the write-ahead log (WAL), and land data in the destination. |
| [`skipprd query`](/cli/query) | Run SQL against your pipelines, interactively or as JSON for scripts. |
| [`skipprd df`](/cli/df) | Print every row of a pipeline or namespace as JSON. |
| [`skipprd serve`](/cli/serve) | Expose your Iceberg tables over Iceberg REST and Flight SQL to tools such as Spark, Trino, and PyIceberg. |
| [`skipprd doctor`](/cli/doctor) | Check `skippr.yml` for mistakes before a long sync. |
| [`skipprd sql-help`](/cli/sql-help) | List the SQL statements `skipprd query` supports, or export them to a file. |
| [`skipprd benchmark`](/cli/benchmark) | Generate a synthetic JSON dataset for performance testing. |

Run `skipprd <command> --help` to see every flag for a command.

## Global flags

Global flags work with every command. You can put them before or after the command name: `skipprd --config prod.yml sync` and `skipprd sync --config prod.yml` are the same.

| Flag | Default | Description |
|---|---|---|
| `--config <PATH>` | `./skippr.yml` | Config file to use. Same as the `SKIPPR_CONFIG_FILE` environment variable. Without it, Skipprd looks in the current directory for `skippr.yml`, then `skippr.yaml`. If the path you pass does not exist, Skipprd falls back to that search. |
| `--log [LEVEL]` | Off | Print logs to stderr. `LEVEL` is `trace`, `debug`, `info`, `warn`, or `error`; `--log` on its own means `info`. Logs are always written to `logs/skipprd.<date>.log`, with or without this flag. |
| `--wal-storage <MODE>` | `disk` | Where the WAL keeps buffered batches: `disk`, `s3`, or `clustered`. Same as `WAL_STORAGE`. See [WAL and buffering](/configuration/buffering). |
| `--wal-s3-bucket <BUCKET>` | State bucket | S3 bucket for WAL segments when `--wal-storage s3`. Same as `SKIPPR_WAL_S3_BUCKET`. A `skippr.wal_s3_bucket` value in `skippr.yml` takes precedence. |
| `--store-type <TYPE>` | `sled` | State store for offsets and run leases: `sled` (the local on-disk state store), `dynamodb`, or `cloud-tables`. Same as `SKIPPR_STORE_TYPE`. A `skippr.store.type` value in `skippr.yml` takes precedence. See [State store](/configuration/skippr-store). |
| `--store-name <NAME>` | None | Table name for the `dynamodb` or `cloud-tables` state store. Same as `SKIPPR_STORE_NAME`. Requires `--store-type` when used with `connect`. |
| `--workspace <NAME>` | None | Written to `skippr.workspace` by `skipprd connect`. Other commands ignore it and read `skippr.yml`. |
| `--storage-mode <MODE>` | None | `local` or `s3`. Written to `skippr.skipprd_el_storage_mode` by `skipprd connect`. Other commands ignore it. |
| `--skippr-s3-bucket <BUCKET>` | None | Written to `skippr.skippr_s3_bucket` by `skipprd connect`. Other commands ignore it. |
| `--tenant <NAME>` | None | Written to `skippr.tenant` by `skipprd connect`. Other commands ignore it. |
| `-h`, `--help` | | Print help for `skipprd` or any command. |
| `-V`, `--version` | | Print the version. Top level only. |

Skipprd also loads a `.env` file that sits next to your `skippr.yml`, so `${NAME}` references in the config can resolve from it.

## Typical workflow

A pipeline goes from nothing to queryable data in five steps. The example uses the public bike-hire sample and keeps engine state on local disk.

1. **Connect a source.** Write the pipeline into `skippr.yml`:

   ```bash
   skipprd --workspace bikehire --storage-mode local connect data-source s3 \
     --pipeline bikehire \
     --name sample \
     --s3-bucket skippr-public-sample-data \
     --s3-prefix bike-hire
   ```

2. **Check the config.**

   ```bash
   skipprd doctor
   ```

3. **Discover the schema.** Skipprd reads the source and saves the inferred columns for each namespace:

   ```bash
   skipprd discover --pipeline bikehire --output text
   skipprd metadata show --pipeline bikehire
   ```

4. **Sync.** Run one pass and exit:

   ```bash
   skipprd sync --pipeline bikehire --once --output text
   ```

5. **Query.** Tables are named `<pipeline>.<namespace>`:

   ```bash
   skipprd query --plain --sql "SELECT count(*) FROM bikehire.rides"
   ```

The same workflow in Python:

::: code-group

```python [Python]
import skippr

session = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
print(session.doctor())
session.discover()
session.sync(once=True)
print(session.query("SELECT count(*) FROM bikehire.rides"))
```

```bash [CLI]
skipprd doctor
skipprd discover --pipeline bikehire
skipprd sync --pipeline bikehire --once
skipprd query --plain --sql "SELECT count(*) FROM bikehire.rides"
```

:::

## Output modes

Pick the output that suits who is reading it:

| Command | Modes | Use for scripts |
|---|---|---|
| `discover`, `sync` | `--output progress` (default; a live progress display on a terminal), `text`, `json` | `--output json` prints one JSON event per line. |
| `doctor` | `--output text` (default), `json` | `--output json` |
| `query` | Interactive table view (default), `--plain` | `--plain` prints one JSON document: `{"header": [...], "rows": [[...]]}`. |
| `df` | JSON only | Same shape as `query --plain`. |
| `metadata show`, `metadata apply` | JSON only | Always JSON. |

## Exit codes

| Code | Meaning |
|---|---|
| `0` | The command succeeded. |
| `1` | The command failed: invalid or unreadable config, a missing `${NAME}` variable, a failed check, sync, discover, or query. |
| `2` | The command line was invalid, for example an unknown flag or a missing required flag. |

Some failures are only described in the logs. If a command exits `1` without explaining why, run it again with `--log`.

## Next steps

- [Install](/getting-started/install)
- [skippr.yml reference](/configuration/skippr-yml)
- [How Skipprd works](/concepts/how-it-works)
- [Python](/python)
