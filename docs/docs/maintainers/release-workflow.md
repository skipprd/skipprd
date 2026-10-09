# Release Workflow

Python wheels publish from `.github/workflows/ci.yml` (**Python CI/CD Pipeline**). Host **Rust CI/CD Pipeline** (`.github/workflows/rust.yml`) builds, tests, chaos-tests, and publishes skipprd on GitHub-hosted Linux x86 (`ubuntu-latest`) plus macOS arm64 on the self-hosted `skippr-darwin-arm64` runner. Both pipelines run on tags (and `workflow_dispatch`), not on `main` / master pushes. `publish_runtime_plugins` uploads the workspace plugin catalog after both architecture builds. `publish_skipprd` runs after `chaos_mode_test`, the GitHub runner e2e jobs (file/Duckdb, SkipprLake, Postgres CDC upsert, S3 schema evolution, file/Postgres), the Darwin host build, and plugin publish. Windows is not a publish target.

## High-level flow

1. `plan_runtime_plugin_release`
2. target-specific build jobs
3. host and package test/compile jobs
4. `publish_runtime_plugins`
5. runtime e2e scenario jobs
6. `publish_skipprd`

## 1. Plan which plugins need rebuilding

`plan_runtime_plugin_release` runs:

```bash
python3 .github/scripts/runtime_plugin_release_plan.py --workspace "$GITHUB_WORKSPACE"
```

The planner compares the workspace plugin catalog against:

```text
https://install.skippr.io/releases/runtime-plugins/latest/manifest-index.json
```

A plugin package is selected when:

- it has never been published
- its crate version changed
- its build checksum changed for the existing published version

This is why plugin versions live in each plugin crate's `Cargo.toml`, not in a shared bundle version.

`build_checksum` is intentionally narrow: it tracks the plugin crate source tree plus `plugins/shared/`, so semver clobber decisions follow plugin code changes rather than unrelated workspace churn.

## 2. Build the selected host and plugin artifacts

Platform build jobs compile the host binary plus **every** workspace catalog plugin (not an e2e subset):

- `linux_x86` on GitHub-hosted `ubuntu-latest`
- `macos_arm64` on the self-hosted `skippr-darwin-arm64` runner

`rust-build-release` loads that catalog via `catalog_package_names`. Windows is not a publish target.

On tag builds, `set_root_package_version.py` stamps the root host package version from the tag name (`1.2.3`) before packaging. It does not stamp `skipprd-python` or `pyproject.toml`. Engine tags are unprefixed (`0.0.0`, `0.1.0`).

## Python wheels (PyPI)

Python has its own semver in `pyproject.toml` and `python/Cargo.toml` (`18.1.2` today). It is not the skipprd git tag.

`.github/workflows/ci.yml` (**Python CI/CD Pipeline**) builds and tests the `skippr` wheel on GitHub-hosted Linux x86 (`ubuntu-latest`) and macOS arm64 (`skippr-darwin-arm64`) for engine tags (`[0-9]*`) and `python-v*` tags, plus `workflow_dispatch`. It does not run on `main` / master or pull requests. Release `cdylib` builds use `[profile.release]` `debug = false` and `strip = "symbols"` so the wheel stays under the PyPI project file limit of 100 MB (`scripts/test-python.sh` fails the job if a wheel is larger).

`python-publish` runs on the same unprefixed engine tags as `publish_skipprd` (not `0.0.0`, not `test*`, not `python-v*`). It uploads wheel filenames that are not yet on PyPI (so Darwin can land after Linux on the same semver). PyPI never reuses a filename: a re-tag of a semver whose current filenames already exist stamps a PEP 427 build number (`skippr-18.0.0-1-cp310-abi3-…`). `pip install skippr==18.0.0` then installs the highest build. Do not bump `pyproject.toml` just to clobber.

Publish uses **PyPI Trusted Publishing** (GitHub OIDC), not a pip login or `PYPI_API_TOKEN`. The job sets `id-token: write` and calls `pypa/gh-action-pypi-publish` with `attestations: false` (GitHub-hosted `ubuntu-latest`). GitHub mints a short-lived token; PyPI accepts it because this repo's GitHub publisher is registered.

Registered publisher on [pypi.org](https://pypi.org):

- Project: `skippr`
- Owner: `skipprd`
- Repository: `skipprd`
- Workflow name: `ci.yml`
- Environment: empty (the job must not set `environment:`)

Do not create a PyPI API token. Do not put `TWINE_PASSWORD` in GitHub secrets.

Bump `pyproject.toml` and `python/Cargo.toml` together, then tag the engine (`18.1.2`). Scratch `0.0.0` does not publish the wheel. `python-v*` still builds and tests the wheel; it does not publish.

## 3. Compile and test the important boundaries

`.github/workflows/rust.yml` (**Rust CI/CD Pipeline**) is the linux x86 plus Darwin arm64 release lane. It runs on unprefixed engine tags (`18.1.2`) and `workflow_dispatch`, not on `main` or master branch pushes.

- `linux_test_suite` — Python workflow contracts (`test_rust_ci.py`, harness/plugin/host-boundary scripts). Cargo tests are skipped for now because they take too long on GitHub-hosted runners.
- `linux_x86` — `rust-build-release` of skipprd plus the full workspace plugin catalog; starts in parallel with `linux_test_suite`
- `macos_arm64` — the same catalog build on the self-hosted `skippr-darwin-arm64` runner
- `chaos_mode_test` — `bike_hire_many` reads 5,100,000 mixed-size bike-hire JSON objects from R2 (`skippr-e2e-sample-data/bike-hire/`) into SkipprLake on the runner (DynamoDB Local catalog, `file://` warehouse) and asserts that exact row count. Mid-sync SIGKILL chaos is off on GitHub-hosted runners so the 5.1M ingest can finish. Pipeline buffers hold until 4 GiB or 7200s so R2 ingest is not overlapping SkipprLake compact; drain then compact in `WAL_COMPACTION_GROUP_MAX_PARTS=16` / 8 MiB groups on GitHub-hosted `ubuntu-latest` (4 cores / 16 GB). The skipprd org is GitHub Free, so larger 8-core hosted runners are not available. R2 secrets only; no AWS.
- `e2e_file_duckdb` — File source append into Duckdb Iceberg
- `e2e_skipprlake` — File source into SkipprLake (`tests/skipprlake_e2e`), including atomic dbt replace
- `e2e_postgres_cdc` — Postgres snapshot-then-CDC upsert into SkipprLake
- `e2e_s3_schema_evolution` — S3 v1 then backward-compatible v2 into SkipprLake
- `e2e_s3_schema_alter` — S3 fixture into SkipprLake, then the enumerated `ALTER TABLE` matrix (rename / merge / drop / promote / fail-closed)
- `e2e_file_postgres` — File source append into Docker Postgres
- `publish_runtime_plugins` — on engine tags, after linux and Darwin catalog builds, stage and upload protocol-matching manifests plus binaries
- `publish_skipprd` — on engine tags, after chaos, the GitHub e2e jobs, Darwin, and plugin publish, upload `skipprd-linux_x86.tar.gz` and `skipprd-macos_arm64.tar.gz` to the install CDN and create the GitHub release

`check_host_dependency_boundaries.py` and `test_runtime_e2e_harness.py` run on that test lane. Windows is not a publish target.

## 4. Publish runtime plugin manifests and binaries

`publish_runtime_plugins` runs:

```bash
python3 .github/scripts/publish_runtime_plugins.py \
  --bucket "$RELEASES_BUCKET" \
  --subdir "$RUNTIME_PLUGIN_RELEASE_SUBDIR" \
  --public-base-url "https://install.skippr.io/releases/$RUNTIME_PLUGIN_RELEASE_SUBDIR" \
  --workspace "$GITHUB_WORKSPACE" \
  --output-dir "$GITHUB_WORKSPACE/runtime-plugin-release"
```

The staged release is then validated before upload:

- every catalog entry must have a generated manifest
- every manifest must carry the workspace runtime protocol version
- `latest/manifest-index.json` must match the current workspace catalog

Only after that validation does CI upload the staged runtime plugin tree to the releases bucket. Tag builds also write `{semver}/manifest-index.json` next to `latest/` so Cloud Rescue bake can pin `skipprd_version` without using `/latest/`.

## 5. Run the runtime acceptance jobs

After runtime plugins are published, the workflow runs the release-style acceptance jobs:

- `published_runtime_plugins_smoke_test` to verify published runtime plugin download behavior
- `bike_hire_test` using the baseline `bike_hire` scenario
- `chaos_mode_test` using `bike_hire_many`
- `s3_wal_test` using `bike_hire_s3_wal_many`
- `deadletters_test`

Together these cover baseline runtime execution, exactly-once behavior under load, deadletters, runtime download behavior, and the current release topology. The baseline `bike_hire` run remains useful because it exercises the simplest release-shaped path without the extra load, WAL, or deadletter variations layered on top.

The post-publish acceptance path verifies artifact download behavior with `artifacts[*].sha256`. It does not re-check `build_checksum` after publish.

## 6. Publish the host binary

`publish_skipprd` then:

- uploads linux and Darwin host archives plus `install.sh` to the GitHub release
- copies those host tarballs into the install releases bucket
- updates the latest host pointer

Host publishing is intentionally separate from runtime plugin manifest publishing. The host is not stamped with plugin versions and should resolve runtime plugins from the published registry by default.

## Release discipline

Use these rules when preparing a release:

- bump an individual plugin crate version when its behavior changes in a way that should force republishing
- do not reintroduce committed runtime manifest templates
- do not bundle runtime plugin binaries beside `skipprd`
- prefer the latest registry index by default; only pin individual plugin versions when intentionally testing or rolling a plugin

That keeps the runtime plugin system behaving like a package manager rather than a monolithic host bundle.
