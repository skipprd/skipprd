# CLI Overview

This repository is the **`skipprd`** engine. The Cloud CLI `skippr` is a different product. Data Engineer commands live in **`sde`**.

`sde discover` and `sde sync` invoke `skipprd` on PATH. Engine maintainers run `skipprd` directly.

## `skipprd`

- `skipprd discover`
- `skipprd sync`
- `skipprd metadata`
- engine SQL and schema dump for plugin testing

## `sde`

Install from [skipprd/sde](https://github.com/skipprd/sde). Docs: [data-engineer.skippr.io](https://data-engineer.skippr.io).

- `sde init`
- `sde connect source ...`
- `sde connect warehouse ...`
- `sde discover` (invokes `skipprd`)
- `sde sync` (invokes `skipprd`)
- `sde model`
- `sde query`
- `sde dbt`
- `sde test`
- `sde vector`
- `sde agent` (alias: `sde chat`)
- `sde doctor`
