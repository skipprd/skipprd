# CLI

Skipprd is the engine binary. The public commands are `discover`, `schema`, and `sync`.

## Commands

| Command | Job |
|---|---|
| [`discover`](discover.md) | Infer source schema. Does not write to the destination. |
| [`schema`](schema.md) | Print the discovered schema. |
| [`sync`](sync.md) | Ingest through the WAL into the configured sink. |

All commands read `skippr.yml` in the working directory, or `--config path/to/skippr.yml`.

```bash
skipprd discover --pipeline bikehire --log
skipprd schema --pipeline bikehire
skipprd sync --pipeline bikehire --once --log
```
