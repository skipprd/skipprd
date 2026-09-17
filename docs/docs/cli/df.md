# Skipprd df

`SELECT *` on engine query views. Same as Python `Session.df()`. Live WAL, unioned with the Skippr datalake when that pipeline has one. No sink and no lake → WAL only.

## Usage

```bash
skipprd df --pipeline <name> [--namespace <ns>] [--plain]
skipprd --config skippr.yml df --pipeline bikehire
skipprd df --pipeline bikehire --namespace orders
skipprd df --namespace bikehire.orders
```

## Flags

| Flag | Required | Description |
|---|---|---|
| `--pipeline, -p` | No | Pipeline name. Falls back to `PIPELINE_NAME`. Required when `--namespace` is a bare namespace (not `pipeline.namespace`). |
| `--namespace` | No | Namespace, or `pipeline.namespace` FQN (same two-part split as `skipprd query`). Omit to read every namespace for the session pipeline. |
| `--plain` | No | Print rows without table formatting. |
| `--log` | No | Enable logging. |

```bash
skipprd df --pipeline bikehire
skipprd df --pipeline bikehire --namespace orders
skipprd df --namespace bikehire.orders
```

`--namespace orders` needs `--pipeline` (or `PIPELINE_NAME`). `--namespace bikehire.orders` is the query FQN.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Missing pipeline, unknown namespace, or query error |
