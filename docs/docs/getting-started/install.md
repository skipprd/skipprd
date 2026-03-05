# Installation

## Install script

```bash
curl -sL "https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh" | sudo bash
```

This detects your platform (macOS x86/arm64, Linux x86/arm64), downloads the latest release from GitHub, and installs the binary to `/usr/local/bin`. The version is printed on success.

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
```

## Prerequisites

Skippr writes to AWS S3 and manages tables via AWS Glue. You need:

- AWS credentials with access to S3 and Glue (set via `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, or an instance profile)
- `AWS_DEFAULT_REGION` set to your target region

No other runtime dependencies are required. Skippr is a single static binary.
