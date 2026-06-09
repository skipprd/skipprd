# CLI Overview

Skippr ships two binaries that read the same `skippr.yml`.

## `skippr`

Use `skippr` for the full product CLI:

- `skippr init`
- `skippr connect source ...`
- `skippr connect warehouse ...`
- `skippr discover`
- `skippr sync`
- `skippr query`
- `skippr model`
- `skippr dbt`
- `skippr vector`
- `skippr chat`
- `skippr doctor`

## `skipprd`

Use `skipprd` when you only need the lightweight engine/runtime path:

- `skipprd discover`
- `skipprd sync`
- `skipprd query`
- `skipprd schema`
- `skipprd sql-help`
- `skipprd benchmark`

`skipprd` is useful for Lambda images, runtime plugin testing, and focused engine debugging.

## Shared engine commands

For engine commands, both binaries consume the same config:

```bash
skippr --config skippr.yml discover --pipeline my_pipeline
skipprd --config skippr.yml discover --pipeline my_pipeline

skippr --config skippr.yml sync --pipeline my_pipeline --once
skipprd --config skippr.yml sync --pipeline my_pipeline --once
```

Commands such as `model`, `dbt`, `vector`, `chat`, auth, and data-engineering diagnostics are `skippr`-only.
