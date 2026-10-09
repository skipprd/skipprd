---
title: Install Skipprd
description: Install the skipprd command with Homebrew or install.sh, or the skippr Python package with pip, on macOS arm64 or Linux x86_64.
---

# Install

Install the `skipprd` command, the `skippr` Python package, or both. They run the same engine, so a `skippr.yml` written by one works with the other.

| You want to | Install |
|---|---|
| Run pipelines from a terminal, cron, or a container | `skipprd` with Homebrew or `install.sh` |
| Build and run pipelines from Python or a notebook | `skippr` with `pip` |

Installing Skipprd means accepting the [Skipprd EULA](https://skippr.io/terms/eula).

## Before you begin

- A **macOS arm64** (Apple silicon) or **Linux x86_64** machine. Other platforms are not published yet.
- For `pip`: Python 3.10 or later.
- HTTPS access from that machine to `install.skippr.io`. Skipprd downloads each connector the first time a pipeline uses it.
- Network access from that machine to your source and destination.

## Install the CLI

::: code-group

```bash [Homebrew]
brew tap skipprd/tap
brew install skipprd
```

```bash [install.sh]
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | sh
```

:::

Homebrew is the simplest path on a Mac. It also works on Linux when `brew` is on your `PATH`.

The install script detects your platform, downloads the latest release from `install.skippr.io`, and writes `skipprd` to `/usr/local/bin`. If that directory is not writable it asks for `sudo`. To install without root, choose a directory on your `PATH`:

```bash
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | SKIPPR_INSTALL_DIR="$HOME/.local/bin" sh
```

To pin a release, for example on a production host, set `SKIPPR_VERSION` to an unprefixed version number:

```bash
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | SKIPPR_VERSION=6.10.0 sh
```

You can also download the Linux x86_64 or macOS arm64 archive from [GitHub Releases](https://github.com/skipprd/skipprd/releases) and put `skipprd` on your `PATH` yourself.

## Install the Python package

```bash
pip install skippr
```

The package includes type stubs, so your editor and `mypy` see every connector class and field. It depends on `pyarrow`; query results come back as Arrow tables. For pandas conversion, install the extra:

```bash
pip install "skippr[pandas]"
```

The Python package does not add the `skipprd` command. Install the CLI as well if you want both.

## Verify the install

::: code-group

```bash [CLI]
skipprd --version
```

```bash [Python]
python -c "import skippr; print(skippr.Session)"
```

:::

`skipprd --version` prints the version number. The Python check prints `<class 'skippr.Session'>`.

## Connectors

Connectors are not bundled with the engine. The first time a pipeline uses a source or destination, Skipprd downloads that connector from `install.skippr.io` into the connector cache (`~/.skippr/runtime_plugins`). Later runs use the cache.

To keep the cache somewhere else — for example, on a persistent volume in a container — set `SKIPPR_RUNTIME_PLUGIN_DIR`:

```bash
export SKIPPR_RUNTIME_PLUGIN_DIR="/var/lib/skippr/connectors"
```

## Upgrade

::: code-group

```bash [Homebrew]
brew upgrade skipprd
```

```bash [install.sh]
curl -sL https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh | sh
```

```bash [pip]
pip install --upgrade skippr
```

:::

## Troubleshooting

- **`skipprd: command not found`** — the install directory is not on your `PATH`, or you installed only the Python package. Run `echo $PATH` and add the directory you installed to (for example, `export PATH="$HOME/.local/bin:$PATH"`), or install the CLI with Homebrew or `install.sh`.
- **`macOS x86_64 release assets are not published yet`** or **`Linux arm64 release assets are not published yet`** — the installer found an unsupported CPU. Run Skipprd on an Apple silicon Mac or an x86_64 Linux machine.
- **`Windows detected`** — Windows builds are not published. Run Skipprd on a macOS or Linux machine.
- **`release version must be unprefixed semver`** — `SKIPPR_VERSION` includes a prefix. Use `6.10.0`, not `v6.10.0`.
- **`Download did not return a tar.gz archive`** — no release exists for that version and platform. Check the version number, or unset `SKIPPR_VERSION` to install the latest release.
- **`unsupported SKIPPR_BINARY`** — this installer only installs `skipprd`. Unset `SKIPPR_BINARY`.
- **`pip` cannot find a matching distribution** — wheels are published for macOS arm64 and Linux x86_64 on Python 3.10 or later. Check `python --version` and your platform.
- **A pipeline hangs or fails on first run while fetching a connector** — the machine cannot reach `install.skippr.io` over HTTPS. Test from the machine that runs the pipeline: `curl -I https://install.skippr.io`.

## Next steps

- [Quickstart: S3 to Athena](/getting-started/quickstart) — land sample data in a queryable AWS table.
- [Quickstart: Snowflake](/getting-started/quickstart-snowflake), [PostgreSQL](/getting-started/quickstart-postgres), or [BigQuery](/getting-started/quickstart-bigquery).
- [Python](/python) — build and run pipelines from code.
- [How Skipprd works](/concepts/how-it-works) — what happens to your data between source and destination.
