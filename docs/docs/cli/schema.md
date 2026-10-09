---
title: skipprd schema
description: Print the column names and types of the Parquet files a pipeline has written to its local data directory.
---

# skipprd schema

`skipprd schema` prints the columns and Arrow types of the Parquet files in a pipeline's local output folder, one `name: type` line per column, sorted by name. Use it to check what actually landed on disk when you write Parquet locally, for example with the [Local file](/connectors/outputs/file) destination. To see the schema `discover` inferred and saved for any pipeline, use [`skipprd metadata show`](/cli/metadata) instead; it works before your first sync and for every destination.

## Usage

```bash
skipprd schema --pipeline <name> [--config <path>] [--log [LEVEL]]
```

There is no Python equivalent. In Python, run `session.query(...)` or `session.df()` and inspect the returned Arrow table's `schema`.

## Options

| Flag | Default | Description |
|---|---|---|
| `-p`, `--pipeline <NAME>` | Required | Pipeline whose output to read. Unlike `discover` and `sync`, `schema` does not fall back to `PIPELINE_NAME`. |
| `--config <PATH>` | `./skippr.yml` | Config file to read. |
| `--log [LEVEL]` | Off | Print logs to stderr, including the folder being read. |
| `--wal-storage <MODE>` | `disk` | Must not be `clustered`. |

See [CLI overview](/cli/overview) for the other global flags.

### Where it reads

`schema` reads every `.parquet` file under `<data dir>/<workspace>_<pipeline>/output`. The data dir is the pipeline's `data_dir` in `skippr.yml`, or the `DATA_DIR` environment variable, default `./data`. For workspace `demo` and pipeline `rides`, that is `./data/demo_rides/output`.

To make the Local file destination write there, set its `output_dir` to that folder.

## Examples

### Print the schema of local Parquet output

```bash
skipprd schema --pipeline rides
```

```text
duration_s: Int64
member: Boolean
ride_id: Int64
station: Utf8
```

### Show the folder being read

```bash
skipprd schema --pipeline rides --log
```

The log line `Querying data dir: ./data/demo_rides/output` shows exactly which folder `schema` scanned.

### See the discovered schema instead

```bash
skipprd metadata show --pipeline rides
```

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Panics with `listing table: Internal("No schema provided.")` and exits `101` | There are no Parquet files in the output folder. This is normal for destinations that write elsewhere (warehouses, S3, Iceberg). | Use `skipprd metadata show` to see the discovered schema, or query the data with `skipprd query`. |
| `error: the following required arguments were not provided`, followed by `--pipeline <PIPELINE>` | `schema` needs `--pipeline` on the command line. | Pass `--pipeline <name>`. |
| `WAL_STORAGE=clustered does not support schema` | `schema` does not run in clustered mode. | Run it without `--wal-storage clustered`. |

## Next steps

- [skipprd metadata](/cli/metadata)
- [skipprd query](/cli/query)
- [Local file destination](/connectors/outputs/file)
