# Runtime E2E Harness

The runtime e2e harness lives at `.github/scripts/runtime_e2e_harness.py`. It is the shared entrypoint for local maintainer testing and the AWS-backed runtime plugin jobs in CI.

## What it covers

The harness validates the release path that matters after the hard cutover:

- published or staged runtime manifest discovery
- download and execution of runtime source, sink, and schema plugins
- chaos-mode crash recovery
- deadletter routing
- Soda assertions for the long-running acceptance cases

The main scenarios are:

| Scenario | Purpose |
|---|---|
| `bike_hire` | Fast, high-signal S3 -> Athena/Glue path |
| `bike_hire_many` | Chaos-mode exactly-once validation across repeated crashes |
| `bike_hire_s3_wal_many` | Chaos-mode validation with S3-backed WAL |
| `deadletters_test` | Deadletter routing, schema sync, and queryability |

## Commands

List scenarios:

```bash
python3 .github/scripts/runtime_e2e_harness.py list
```

Clean shared AWS state:

```bash
python3 .github/scripts/runtime_e2e_harness.py prepare-aws-state
```

Run one or more scenarios:

```bash
python3 .github/scripts/runtime_e2e_harness.py run bike_hire deadletters_test --mode smoke
```

Stage a local release-like manifest tree for the current S3/Athena/Glue runtime plugins:

```bash
python3 .github/scripts/runtime_e2e_harness.py stage-local-runtime-release \
  --skippr-el target/debug/skippr-el
```

Validate published runtime plugin releases directly:

```bash
python3 .github/scripts/runtime_e2e_harness.py runtime-plugins \
  --mode smoke \
  --architecture-name linux_x86 \
  --releases-bucket skippr-web-install-site-prod
```

## Smoke vs full

- `smoke` is for fast local confidence. It runs the shortest high-signal checks and avoids the slowest acceptance assertions.
- `full` mirrors the heavier CI behavior, including repeated chaos-mode runs and Soda checks where configured.

For day-to-day development, start with `bike_hire --mode smoke`. Use the chaos scenarios only when you are working on exactly-once, WAL recovery, or deadletter behavior.

## Local staged manifests

The fastest realistic maintainer loop is:

1. build `skippr-el`
2. run `stage-local-runtime-release`
3. copy `skippr-el` into a clean directory
4. run a smoke scenario with `--local-runtime-manifest-dir`

Example:

```bash
python3 .github/scripts/runtime_e2e_harness.py stage-local-runtime-release \
  --skippr-el target/debug/skippr-el

tmpdir="$(mktemp -d)"
mkdir -p "$tmpdir/debug"
cp target/debug/skippr-el "$tmpdir/debug/skippr-el"

python3 .github/scripts/runtime_e2e_harness.py run bike_hire \
  --mode smoke \
  --skippr-el "$tmpdir/debug/skippr-el" \
  --local-runtime-manifest-dir /path/to/staged-local-runtime-release
```

The clean host directory matters because the harness explicitly rejects a host artifact that already contains `skippr-plugin-*` binaries beside it.

## Published version pins

Instead of local manifests, you can pin published plugin versions with repeated `PLUGIN=VERSION` flags:

```bash
python3 .github/scripts/runtime_e2e_harness.py run bike_hire \
  --mode smoke \
  --runtime-plugin-version S3=8.1.0 \
  --runtime-plugin-version Athena=8.1.0 \
  --runtime-plugin-version Glue=8.1.0
```

This is useful when debugging a published regression or validating a release candidate against one plugin at a time.

Do not combine `--runtime-plugin-version` with `--local-runtime-manifest-dir`.

## CI usage

The main workflow uses the harness in two ways:

- scenario jobs such as `bike_hire`, `bike_hire_many`, `bike_hire_s3_wal_many`, and `deadletters_test`
- the `runtime-plugins` acceptance job, which verifies published manifest discovery and release download behavior

That makes the harness the best place to reproduce CI failures locally before pushing fixes.

## Soda bootstrapping

The harness creates its own temporary Python virtualenv for Soda-based checks. Maintainers do not need a global Soda install just to run the full scenarios.
