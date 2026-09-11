# Installation

## Install the product CLI

```bash
curl -sL "https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh" | sudo bash
```

This installs `skippr`, the product CLI. Use it for project setup, connector configuration, discovery, sync, modeling, dbt helpers, vector ingestion, and data-engineering workflows.

## Optional lightweight engine binary

Some deployments only need the engine/runtime path. Install `skipprd` when you want the smaller binary for Lambda images, runtime plugin tests, or focused engine work:

```bash
curl -sL "https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh" | SKIPPR_BINARY=skipprd sudo bash
```

`skipprd` reads the same `skippr.yml`. Product workflows stay on `skippr`; `skipprd discover` and `skipprd sync` invoke this runtime. Direct `skipprd` is for Lambda images, plugin tests, and engine SQL.

### Install a specific version

```bash
curl -sL "https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh" | SKIPPR_VERSION=v6.10.0 sudo bash
```

### Install to a custom directory

If you don't have root access or prefer a user-local install:

```bash
curl -sL "https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh" | SKIPPR_INSTALL_DIR="$HOME/.local/bin" bash
```

## Verify

```bash
skippr --version
skipprd --version  # optional engine binary
```

## Runtime plugin downloads

Skippr does not ship connector binaries beside the CLI. Instead:

- engine commands resolve runtime source, sink, and schema plugins from the latest published manifest index by default
- plugin manifests and binaries are downloaded from `install.skippr.io`
- downloaded artifacts are cached under `~/.skippr/runtime_plugins` unless `SKIPPR_RUNTIME_PLUGIN_DIR` is set
- connector configs can optionally pin an individual plugin `version`

That keeps the host release separate from plugin releases and lets plugins be versioned per crate.

## Prerequisites

Skippr writes to AWS S3 and manages tables via AWS Glue. You need:

- AWS credentials with access to S3 and Glue (set via `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, or an instance profile)
- `AWS_DEFAULT_REGION` set to your target region

No local plugin preinstall is required. Skippr downloads runtime plugin binaries on demand and caches them automatically.
