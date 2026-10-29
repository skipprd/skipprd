# Release Workflow

Operator path is **`/skippr-release`**: bump changed plugin crates, move `CHANGELOG` Unreleased into the tag, commit with the pre-commit hook, wait until SHA batteries are green on this commit (`rust.yml` File smoke + `linux_dst`, `skippr-fuzz.yml`, `skippr-tpc.yml` TPC-H SF10, `perf.yml`, `perf-db.yml`), then tag, push, watch both GitHub Actions pipelines until publish jobs succeed, then surgical public docs via **`skipprd-docs`**. Re-dispatch a workflow unless that SHA already succeeded; `rust.yml` also requires the named File smoke + `linux_dst` jobs. Never skip matrix cells. There is no `skippr-gate`. Everyday plugin commits do not bump crate versions. Clobberable pre-release testing is **`/skippr-beta-release`**: same SHA batteries, then next production patch plus `-beta` (`18.3.10-beta`), full artifact publish (`python-publish` + skipprd host tarballs + runtime plugins), overwrite that tag on every run, skip `latest` pointers, skip skipprd-docs.

Python wheels publish from `.github/workflows/ci.yml` (**Python CI/CD Pipeline**). Host **Rust CI/CD Pipeline** (`.github/workflows/rust.yml`) builds, tests, chaos-tests, and publishes skipprd on GitHub-hosted Linux x86 (`ubuntu-latest`) plus macOS arm64 on the self-hosted `skippr-darwin-arm64` runner. Both pipelines run on tags (and `workflow_dispatch`), not on `main` / master pushes. Local File smoke (`e2e_file_duckdb`, `e2e_file_copy`) then `linux_dst` gates `publish_runtime_plugins`. Product-tag e2e after that download `{tag}/manifest-index.json` from the CDN. `publish_skipprd` runs after CDN e2e, `chaos_mode_published`, protocol-compat jobs, Darwin, `linux_dst`, and plugin publish. Scratch `0.0.0` runs File smoke plus local `chaos_mode_test` and does not publish. Windows is not a publish target.

## High-level flow

1. precommit: `cargo test -p skipprd --lib` (DST World named scripts + bounded proptest)
2. SHA batteries on the commit (workflow_dispatch; skip re-dispatch per workflow when that SHA succeeded; `rust.yml` also requires File smoke + `linux_dst` jobs): `rust.yml` File smoke + `linux_dst`, `skippr-fuzz.yml`, `skippr-tpc.yml` SF10, `perf.yml`, `perf-db.yml`
3. product tag
4. target-specific catalog builds (`linux_x86`, `macos_arm64`) plus Python workflow contracts
5. local File smoke (`e2e_file_duckdb`, `e2e_file_copy`) then `linux_dst` (`sqlrt::dst::`) — and local `chaos_mode_test` on `0.0.0`
6. `publish_runtime_plugins` (product tags only; needs File smoke **and** `linux_dst`)
7. CDN e2e (`chaos_mode_published`, SkipprLake, S3, Postgres) plus protocol-compat jobs
8. `publish_skipprd`

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

If the crate version is unchanged but protocol, checksum, SDK fingerprint, or artifact sha256 differs, the planner fails closed. Bump the plugin crate.

This is why plugin versions live in each plugin crate's `Cargo.toml`, not in a shared bundle version.

`build_checksum` is intentionally narrow: it tracks the plugin crate source tree plus `plugins/shared/`, so semver clobber decisions follow plugin code changes rather than unrelated workspace churn.

## 2. Build the selected host and plugin artifacts

Platform build jobs compile the host binary plus **every** workspace catalog plugin (not an e2e subset):

- `linux_x86` on GitHub-hosted `ubuntu-latest`
- `macos_arm64` on the self-hosted `skippr-darwin-arm64` runner

`rust-build-release` loads that catalog via `catalog_package_names`. Windows is not a publish target.

On tag builds, `set_root_package_version.py` stamps the root host package version from the tag name (`1.2.3`) before packaging. It does not stamp `skipprd-python` or `pyproject.toml`. Engine tags are unprefixed (`0.0.0`, `0.1.0`).

## Python wheels (PyPI)

Python has its own semver in `pyproject.toml` and `python/Cargo.toml` (`18.3.6` today). It is not the skipprd git tag.

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

Bump `pyproject.toml` and `python/Cargo.toml` together, then tag the engine (`18.3.6`). Scratch `0.0.0` does not publish the wheel. `python-v*` still builds and tests the wheel; it does not publish.

## 3. Compile and test the important boundaries

`.github/workflows/rust.yml` (**Rust CI/CD Pipeline**) is the linux x86 plus Darwin arm64 release lane. It runs on unprefixed engine tags (`18.3.6`) and `workflow_dispatch`, not on `main` or master branch pushes.

- `linux_test_suite` — Python workflow contracts (`test_rust_ci.py`, harness/plugin/host-boundary scripts). Does not run `cargo test -p skipprd`.
- `linux_x86` — `rust-build-release` of skipprd plus the full workspace plugin catalog; starts in parallel with `linux_test_suite`
- `macos_arm64` — the same catalog build on the self-hosted `skippr-darwin-arm64` runner
- `e2e_file_duckdb` — local File source append into Duckdb Iceberg; gates plugin publish
- `e2e_file_copy` — local File → File `type: copy`; gates plugin publish
- `linux_dst` — `cargo test -p skipprd --lib -- sqlrt::dst::` after File smoke; gates plugin publish and `publish_skipprd`
- `chaos_mode_test` — local plugins on scratch `0.0.0` only. `bike_hire_many` reads 5,100,000 mixed-size bike-hire JSON objects from R2 (`skippr-e2e-sample-data/bike-hire/`) into SkipprLake on the runner (DynamoDB Local catalog, `file://` warehouse) and asserts that exact row count. Mid-sync SIGKILL chaos is off on GitHub-hosted runners so the 5.1M ingest can finish. Pipeline buffers hold until 4 GiB or 7200s so R2 ingest is not overlapping SkipprLake compact; drain then compact in `WAL_COMPACTION_GROUP_MAX_PARTS=16` / 8 MiB groups on GitHub-hosted `ubuntu-latest` (4 cores / 16 GB). The skipprd org is GitHub Free, so larger 8-core hosted runners are not available. R2 secrets only; no AWS.
- `publish_runtime_plugins` — on product engine tags, after linux and Darwin catalog builds plus File smoke **and** `linux_dst`, stage and upload protocol-matching manifests plus binaries
- `chaos_mode_published` — same `bike_hire_many` path after plugin publish; skipprd downloads `{tag}/manifest-index.json`. Product tags only (not `0.0.0`).
- `e2e_skipprlake` — File source into SkipprLake (`tests/skipprlake_e2e`), including atomic dbt replace, against published plugins
- `e2e_postgres_cdc` — Postgres snapshot-then-CDC upsert into SkipprLake
- `e2e_s3_schema_evolution` — S3 v1 then backward-compatible v2 into SkipprLake
- `e2e_schema_lifecycle` — discover/sync then `skipprd migrate` + `ALTER TABLE` matrix into SkipprLake (rename / merge / drop / promote / fail-closed), with a second sync after apply
- `e2e_file_postgres` — File source append into Docker Postgres
- `e2e_s3_file_copy` — S3 (R2) → File and File → S3 (R2) `type: copy`
- `e2e_s3_s3_copy` — S3 (R2) → S3 (R2) `type: copy` Identity PUT (source and sink plugins together)
- `e2e_prev_plugins` — this tag’s host with previous protocol-compatible File/S3/SkipprLake crate pins
- `e2e_prev_host` — previous tag’s `skipprd-linux_x86` with current crate pins (skips if that GitHub release asset is missing)
- `publish_skipprd` — on engine tags, after CDN e2e, Darwin, `linux_dst`, and plugin publish, upload `skipprd-linux_x86.tar.gz` and `skipprd-macos_arm64.tar.gz` to the install CDN and create the GitHub release

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
- a crate version path that already exists with a different protocol or sha256 is a hard failure

Only after that validation does CI upload the staged runtime plugin tree to the releases bucket. Tag builds write `{semver}/manifest-index.json` next to `latest/`. `aws s3 sync` must not pass `--delete`. Versioned plugin blobs are never overwritten.

## 5. Run the runtime acceptance jobs

After runtime plugins are published, skipprd downloads `{tag}/manifest-index.json` (not local `USE_LOCAL_PLUGIN_CODE` artifacts). The live jobs are:

- `chaos_mode_published` using `bike_hire_many` against CDN plugins
- File → SkipprLake, Postgres CDC, S3 schema evolution / ALTER, File → Postgres, and the S3 copy matrices
- `e2e_prev_plugins` and `e2e_prev_host` when an older same-protocol engine tag exists

Scratch `0.0.0` does not publish plugins, so it keeps local File smoke plus `chaos_mode_test` instead of the CDN jobs.

The post-publish acceptance path verifies artifact download behavior with `artifacts[*].sha256`. It does not re-check `build_checksum` after publish. Do not `aws s3 sync --delete`. Versioned plugin blobs are never overwritten. Athena/Glue e2e stays disabled until AWS secrets exist.

## 6. Publish the host binary

`publish_skipprd` then:

- uploads linux and Darwin host archives plus `install.sh` to the GitHub release
- copies those host tarballs into the install releases bucket
- updates the latest host pointer

Host publishing is separate from runtime plugin binary publishing. Tag builds stamp the skipprd crate version, and that host resolves `{semver}/manifest-index.json` written by the same tag. `/latest/` remains a mutable pointer for installers; it is not the production discovery default.

## Release discipline

Use these rules when preparing a release:

- bump an individual plugin crate version when its behavior or protocol changes — version paths are immutable (`python3 .github/scripts/bump_changed_plugin_crates.py --base <last-engine-tag>` before the tag)
- do not reintroduce committed runtime manifest templates
- do not bundle runtime plugin binaries beside `skipprd`
- do not `aws s3 sync --delete` the runtime-plugins prefix
- YAML `version:` pins a crate for tests; production hosts use the release index

A protocol bump without a crate bump is a release bug.
