---
title: skipprd doctor
description: Check skippr.yml for missing sections, broken references, unset secrets, and mismatched sinks before you run a sync.
---

# skipprd doctor

`skipprd doctor` checks your configuration before you run anything long. It loads `skippr.yml`, resolves every `${NAME}` reference, and checks that each pipeline has a source, that every reference between pipelines and entries resolves, and that paired sinks match. It does not connect to your source or destination, so it is fast and safe to run anywhere: after editing the config, in CI on every change, or as the first step of a deploy. Doctor checks every pipeline in the file.

## Usage

::: code-group

```python [Python]
import skippr

session = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
result = session.doctor()
print(result["ok"])
```

```bash [CLI]
skipprd doctor [--output text|json] [--config <path>]
```

:::

## Options

| Flag | Default | Description |
|---|---|---|
| `--output <MODE>` | `text` | `text` prints one line per check. `json` prints `{"ok": bool, "checks": [...]}` for scripts. |
| `--config <PATH>` | `./skippr.yml` | Config file to check. |
| `--log [LEVEL]` | Off | Print logs to stderr. |

See [CLI overview](/cli/overview) for the other global flags.

### What doctor checks

| Check | Passes when |
|---|---|
| Config loads | `skippr.yml` parses and every `${NAME}` reference has a value. If not, doctor stops with `[skippr] config failed: ...` before running other checks. |
| Pipelines | At least one pipeline is configured. |
| Sources | `data_sources` is not empty. |
| Destinations | Always passes. A pipeline without a `data_sink` is valid: its data stays in the WAL. |
| Each pipeline | Its `data_source`, `data_sink`, and other references point at entries that exist. |
| Whole config | No two SkipprLake or DuckDB destinations write the same namespace, and paired sinks (SkipprLake, AthenaIceberg, DuckDB) match their schema sink. |
| Storage mode | Reports `skipprd_el_storage_mode` for each pipeline. |

Doctor validates configuration only. To test credentials and connectivity, run [`skipprd discover`](/cli/discover), which reads the source but never writes to the destination.

## Examples

### Check the config in the current directory

```bash
skipprd doctor
```

```text
  [ok]   1 pipeline(s) configured
  [ok]   data_sources configured
  [ok]   data_sinks configured (optional)
  [ok]   pipeline 'bikehire' (source+sink)
  [ok]   skipprd_el_storage_mode=local

All checks passed.
```

A failing check is marked `[FAIL]` and the summary reads `Some checks failed.`

### JSON for CI

```bash
skipprd doctor --output json
```

```json
{
  "ok": true,
  "checks": [
    { "ok": true, "severity": "info", "message": "1 pipeline(s) configured" },
    { "ok": true, "severity": "info", "message": "data_sources configured" },
    { "ok": true, "severity": "info", "message": "no data_sink; WAL is the dataset" },
    { "ok": true, "severity": "info", "message": "pipeline 'rides' (source, WAL-only)" },
    { "ok": true, "severity": "info", "message": "skipprd_el_storage_mode=local" }
  ]
}
```

Each check has `ok`, `severity` (`info` or `error`), and `message`. A check may also include `suggested_fix_command`.

### Fail a CI job on a bad config

```bash
skipprd --config deploy/skippr.yml doctor --output json > doctor.json || {
  jq -r '.checks[] | select(.ok == false) | .message' doctor.json
  exit 1
}
```

Doctor needs every `${NAME}` variable to be set, so export your secrets (or test placeholders) in the CI job first.

## Troubleshooting

| Output | Cause | Fix |
|---|---|---|
| `[skippr] config failed: ... references ${NAME} at ... but that environment variable is not set` | A secret reference has no value. | `export NAME=...`, or add it to a `.env` file next to `skippr.yml`. |
| `[FAIL] no pipelines configured` and `[FAIL] data_sources is empty` | Doctor found no config file, or the file is empty. | Run from the folder that contains `skippr.yml`, or pass `--config`. |
| `[FAIL] ... must be equal ...` | A paired data sink and its schema sink differ. | Make the two blocks identical, or re-add them with [`skipprd connect`](/cli/connect). |
| `[skippr] config failed: Invalid Skippr configuration in ...` | `skippr.yml` has a YAML error or an unknown key. | Fix the line named in the message. |

## Exit codes

| Code | Meaning |
|---|---|
| `0` | All checks passed. |
| `1` | At least one check failed, or the config could not be loaded. |

## Next steps

- [skippr.yml reference](/configuration/skippr-yml)
- [skipprd connect](/cli/connect)
- [skipprd discover](/cli/discover)
