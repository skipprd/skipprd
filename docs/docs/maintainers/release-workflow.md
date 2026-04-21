# Release Workflow

The main release pipeline is `.github/workflows/build-publish.yml`. It is tag-driven and publishes the host binary and runtime plugin registry as separate artifacts.

## High-level flow

1. `plan_runtime_plugin_release`
2. target-specific build jobs
3. host and package test/compile jobs
4. `publish_runtime_plugins`
5. runtime e2e scenario jobs
6. `publish_skippr_el`

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

Platform build jobs produce the host binary plus the runtime plugin binaries needed for release staging:

- `linux_x86`
- `macos_arm64`
- `windows_x86`

On tag builds, `set_root_package_version.py` stamps the root host package version from the tag name before packaging `skippr-el`.

## 3. Compile and test the important boundaries

The workflow currently uses:

- full host test execution on Linux
- postgres sink tests on Linux
- compile-only validation for the same host and postgres plugin targets on macOS and Windows
- `check_host_dependency_boundaries.py` on all major build/test lanes

This combination keeps CI focused on the host/package boundary and catches the common refactor regressions without having to execute the full runtime e2e matrix everywhere.

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

Only after that validation does CI upload the staged runtime plugin tree to the releases bucket.

## 5. Run the runtime acceptance jobs

After runtime plugins are published, the workflow runs the release-style acceptance jobs:

- `linux_x86_test` using the `bike_hire` scenario plus runtime plugin release download checks
- `chaos_mode_test` using `bike_hire_many`
- `s3_wal_test` using `bike_hire_s3_wal_many`
- `deadletters_test`

These are the final gates for exactly-once, deadletters, runtime download behavior, and the current release topology.

The post-publish acceptance path verifies artifact download behavior with `artifacts[*].sha256`. It does not re-check `build_checksum` after publish.

## 6. Publish the host binary

`publish_skippr_el` then:

- uploads host archives plus `install.sh` to the GitHub release
- copies host tarballs into the install releases bucket
- updates the latest host pointer

Host publishing is intentionally separate from runtime plugin manifest publishing. The host is not stamped with plugin versions and should resolve runtime plugins from the published registry by default.

## Release discipline

Use these rules when preparing a release:

- bump an individual plugin crate version when its behavior changes in a way that should force republishing
- do not reintroduce committed runtime manifest templates
- do not bundle runtime plugin binaries beside `skippr-el`
- prefer the latest registry index by default; only pin individual plugin versions when intentionally testing or rolling a plugin

That keeps the runtime plugin system behaving like a package manager rather than a monolithic host bundle.
