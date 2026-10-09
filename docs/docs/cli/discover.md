---
title: skipprd discover
description: Read a pipeline's source and infer the schema of every namespace, without writing anything to your destination.
---

# skipprd discover

`skipprd discover` reads a pipeline's source, infers the columns and types of each namespace (a stream of records with one schema, such as a table or an event type), and saves that schema. It never writes to your destination, so it is safe to run against production sources. Run it after you connect a new source and before your first sync. You can run it again at any time: new fields are added to the saved schema. If you skip it, `skipprd sync` runs discover for you the first time.

## Usage

::: code-group

```python [Python]
import skippr

session = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
session.discover()
```

```bash [CLI]
skipprd discover --pipeline <name> [--output progress|text|json] [--log [LEVEL]] [--config <path>]
```

:::

## Options

| Flag | Default | Description |
|---|---|---|
| `-p`, `--pipeline <NAME>` | `PIPELINE_NAME` env var | Pipeline to discover. Required: if neither is set, discover does nothing. |
| `--output <MODE>` | `progress` | `progress` shows a live progress display on a terminal. `text` prints one plain line per event. `json` prints one JSON object per line for scripts. |
| `--config <PATH>` | `./skippr.yml` | Config file to read. |
| `--log [LEVEL]` | Off | Print logs to stderr. `--log` alone means `info`. Turning on logs hides the progress display. |
| `--wal-storage <MODE>` | `disk` | Must not be `clustered`; discover does not run in clustered mode. |

See [CLI overview](/cli/overview) for the other global flags.

## What discover does

1. Loads the schema already saved for the pipeline, if there is one.
2. Reads records from the source and infers the type of every field, including nested fields.
3. Saves the updated schema to the pipeline's state storage: local disk when `skipprd_el_storage_mode` is `local`, otherwise your state bucket.
4. Cleans up its temporary working data and exits. Nothing is sent to the destination.

## Examples

### Discover with plain output

```bash
skipprd discover --pipeline bikehire --output text
```

Output looks like this (namespace names and counts come from your data):

```text
Discover started: pipeline=bikehire
[start] Discovering
[done]  Discovering
Namespace discovered: rides (14 fields)
Schema evolved: rides
Discover complete: pipeline=bikehire namespaces=1 fields=14 elapsed=5321ms
```

`Schema evolved` appears whenever a namespace gained, lost, or changed fields compared with the saved schema, including on the first run.

### Structured output for scripts

```bash
skipprd discover --pipeline bikehire --output json
```

Each line is one event. The events are `discover_start`, `namespace_discovered` (with `namespace` and `field_count`), `schema_evolved` (with `fields_added`), and `discover_complete` (with `ok`, `namespaces_discovered`, `total_fields`, and `elapsed_ms`). Every event also has `run_id`, `phase`, and `timestamp`, plus extra context fields your script can ignore. A trimmed `discover_complete` event:

```json
{"event":"discover_complete","phase":"complete","pipeline":"bikehire","namespaces_discovered":1,"total_fields":14,"elapsed_ms":5321,"ok":true,"timestamp":"2026-10-09T13:38:59.142377+00:00"}
```

Read the `event` field to tell events apart rather than relying on line order.

### Review what was discovered

```bash
skipprd metadata show --pipeline bikehire
```

See [skipprd metadata](/cli/metadata) for the output and how to correct a type.

### Use a different config file

```bash
skipprd --config ./prod/skippr.yml discover --pipeline bikehire --log
```

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Exits immediately with no output and code `0` | No pipeline was given. | Pass `--pipeline`, or set `PIPELINE_NAME`. |
| Stops after `[start] Discovering` and exits `1` | The source failed (credentials, network, or a bad setting). The reason is written to the logs. | Rerun with `--log` to see the error on screen, or read `logs/skipprd.<date>.log`. |
| `[skippr] config failed: ... references ${NAME} ... but that environment variable is not set` | A `${NAME}` reference in `skippr.yml` has no value. | `export NAME=...`, or add it to a `.env` file next to `skippr.yml`. |
| `WAL_STORAGE=clustered does not support discover` | Discover runs only with `disk` or `s3` WAL storage. | Run discover without `--wal-storage clustered` (and unset `WAL_STORAGE`). |
| A field has the wrong type | Inference chose a type from the records it saw. | Correct it with [`skipprd metadata apply`](/cli/metadata). |

## Next steps

- [skipprd metadata](/cli/metadata)
- [skipprd sync](/cli/sync)
- [Schema discovery and evolution](/concepts/schema)
