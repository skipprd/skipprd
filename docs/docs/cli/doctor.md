# Skipprd doctor

Preflight the engine config before a long `sync`. Checks `skippr.yml`, env refs, source, optional sink, and WAL. Not `sde doctor` (dbt, login, LLM).

Same checks as Python `Session.doctor()`.

## Usage

```bash
skipprd doctor [--output text|json] [--log [LEVEL]]
skipprd --config skippr.yml doctor [--output json]
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--output` | No | `text` (default) or `json`. JSON shape is `{ "ok": bool, "checks": [ { "name", "ok", "message" } ] }`. |
| `--log` | No | Enable logging. |

`data_sink` is optional. Without a sink, doctor still passes if the WAL is usable — the WAL is the dataset.

## Example

```bash
skipprd --config skippr.yml doctor
skipprd doctor --output json
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | All checks passed |
| 1 | At least one check failed |
