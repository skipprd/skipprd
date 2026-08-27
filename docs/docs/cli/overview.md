# CLI Overview

The public product CLI is `skippr`. It reads one `skippr.yml`. Runtime ingest is implemented by the `skipprd` binary; `skippr discover` and `skippr sync` invoke it. You do not switch CLIs for the same product workflow.

## `skippr`

- `skippr init`
- `skippr connect source ...`
- `skippr connect warehouse ...`
- `skippr discover`
- `skippr sync`
- `skippr model`
- `skippr query`
- `skippr dbt`
- `skippr test`
- `skippr vector`
- `skippr agent` (alias: `skippr chat`)
- `skippr doctor`

## `skipprd`

`skipprd` remains the runtime implementation for Lambda images, plugin testing, and maintainer debugging (`skipprd metadata`, engine SQL, schema dump). Public docs teach `skippr ...`.
