---
title: Install Skipprd
description: Install the Skipprd ELT engine on macOS or Linux, then verify Skipprd discover, Skipprd schema, and Skipprd sync.
---

# Install

Skipprd is the self-hosted ELT engine. After install you run `skipprd discover`, `skipprd schema`, and `skipprd sync` against a `skippr.yml`.

## Prerequisites

- macOS arm64 or Linux x86_64
- Network access to the source, the destination, and `install.skippr.io` (runtime plugins)
- Destination credentials in the environment (warehouse, object store, or database)

Installing Skipprd means accepting the [Skipprd EULA](https://skippr.io/terms/eula).

## Homebrew

```bash
brew tap skipprd/tap
brew install skipprd
skipprd --version
```

## Install script

```bash
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | sh
skipprd --version
```

The script installs `skipprd` to `/usr/local/bin` by default. Override the directory when you do not have root:

```bash
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | SKIPPR_INSTALL_DIR="$HOME/.local/bin" sh
```

Pin a release with `SKIPPR_VERSION`:

```bash
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | SKIPPR_VERSION=v6.10.0 sh
```

## Runtime plugins

Skipprd does not ship connector binaries beside the engine.

- `discover` and `sync` resolve source, sink, and schema plugins from the published manifest index
- artifacts download from `install.skippr.io` and cache under `~/.skippr/runtime_plugins`
- set `SKIPPR_RUNTIME_PLUGIN_DIR` to change the cache
- a connector config may pin `version:` per plugin

## Next

- [Snowflake](/getting-started/quickstart-snowflake)
- [PostgreSQL](/getting-started/quickstart-postgres)
- [BigQuery](/getting-started/quickstart-bigquery)
- [S3 to Athena](/getting-started/quickstart)

## Troubleshooting

- **`unsupported SKIPPR_BINARY`** — this installer only installs `skipprd`.
- **macOS x86_64 / Linux arm64** — those release assets are not published yet. Use arm64 macOS or x86_64 Linux.
- **Windows** — Skipprd release archives are not published for Windows. Run the engine on macOS or Linux.
