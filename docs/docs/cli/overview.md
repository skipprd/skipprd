# CLI

Skipprd is the engine binary. The public commands are `connect`, `discover`, `schema`, `sync`, `query`, `doctor`, and `df`. Python `Session` calls the same engine.

## Commands

| Command | Job |
|---|---|
| [`connect`](connect.md) | Write `skippr.yml` from typed plugin configs. |
| [`discover`](discover.md) | Infer source schema. Does not write to the destination. |
| [`schema`](schema.md) | Print the discovered schema. |
| [`sync`](sync.md) | Ingest through the WAL, then into the sink if one is configured. Without a sink the WAL is the dataset. |
| [`query`](query.md) | SQL against engine views (live WAL, unioned with the datalake when present). |
| [`doctor`](doctor.md) | Preflight: config, source, optional sink, WAL. |
| [`df`](df.md) | `SELECT *` on those query views. Same as `Session.df()`. |

All commands read `skippr.yml` in the working directory, or `--config path/to/skippr.yml`. `--pipeline` names the pipeline.

```bash
skipprd discover --pipeline bikehire --log
skipprd schema --pipeline bikehire
skipprd sync --pipeline bikehire --once --log
skipprd df --pipeline bikehire
```
