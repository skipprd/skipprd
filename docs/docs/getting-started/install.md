# Installation

## Install script

```bash
curl -sL "https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh" | sudo bash
```

This detects your platform (macOS x86/arm64, Linux x86/arm64), downloads the latest release from GitHub, and installs the binary to `/usr/local/bin`. The version is printed on success.

The install script only installs the host binary, `skipprd`. Runtime plugins are resolved on demand the first time a pipeline needs them.

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
skipprd --version
```

## Runtime plugin downloads

Skippr no longer ships connector binaries beside the host. Instead:

- `skipprd` resolves runtime plugins from the latest published manifest index by default
- plugin manifests and binaries are downloaded from `install.skippr.io`
- downloaded artifacts are cached under `~/.skippr/runtime_plugins` unless `SKIPPR_RUNTIME_PLUGIN_DIR` is set
- connector configs can optionally pin an individual plugin `version`

That keeps the host release separate from plugin releases and lets plugins be versioned per crate.

## Prerequisites

Skippr writes to AWS S3 and manages tables via AWS Glue. You need:

- AWS credentials with access to S3 and Glue (set via `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, or an instance profile)
- `AWS_DEFAULT_REGION` set to your target region

No local plugin preinstall is required. The host downloads runtime plugin binaries on demand and caches them automatically.
