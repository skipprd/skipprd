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
  --skipprd target/debug/skipprd
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

## Local plugin code

The fastest realistic maintainer loop is:

1. build `skipprd`
2. run the config-aware local runtime plugin helper
3. set `USE_LOCAL_PLUGIN_CODE=1`
4. run the e2e scenario

Example:

```bash
manifest_dir="$(python3 .github/scripts/local_runtime_plugins.py \
  --config .github/actions/e2e/postgres_iceberg_types_cdc/skipprd.yml \
  --pipeline postgres_iceberg_types_cdc)"

USE_LOCAL_PLUGIN_CODE=1 \
SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR="$manifest_dir" \
python3 .github/scripts/runtime_e2e_harness.py run postgres_iceberg_types_cdc \
  --mode smoke \
  --skipprd target/debug/skipprd
```

The helper builds only the runtime plugins referenced by the chosen config and pipeline. If `USE_LOCAL_PLUGIN_CODE` is unset, the harness validates the published download path as usual.

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

Do not combine published version pins with `USE_LOCAL_PLUGIN_CODE` unless you are explicitly testing fallback behavior for a plugin that has no generated local manifest.

## CI usage

The main workflow uses the harness in two ways:

- scenario jobs such as `bike_hire`, `bike_hire_many`, `bike_hire_s3_wal_many`, and `deadletters_test`
- the `runtime-plugins` acceptance job, which verifies published manifest discovery and release download behavior

That makes the harness the best place to reproduce CI failures locally before pushing fixes.

CI jobs that need current-commit plugin behavior should:

1. build `skipprd`
2. run `.github/scripts/local_runtime_plugins.py --config <scenario skipprd.yml> --pipeline <pipeline>`
3. export `USE_LOCAL_PLUGIN_CODE=1`
4. export `SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR` to the helper output
5. run the harness scenario

Release-style acceptance jobs should leave `USE_LOCAL_PLUGIN_CODE` unset so they continue to verify published manifest discovery and download behavior.

## Soda bootstrapping

The harness creates its own temporary Python virtualenv for Soda-based checks. Maintainers do not need a global Soda install just to run the full scenarios.
