# Runtime Plugins

Skippr treats runtime plugins as independently versioned packages. The host binary is published separately, while source, sink, and schema plugins are resolved from published manifests by default.

The runtime boundary is intentionally narrow:

- the **host** owns WAL durability, recovery, schema distribution, and offset materialization
- **source plugins** read external systems and emit work to the host
- **sink/schema plugins** perform destination-specific writes that must tolerate replay

The exact contract is documented in [Runtime Plugin Contract](runtime-plugin-contract.md).

For API/SaaS marketing and analytics connectors, see [API / SaaS source plugins](./api-saas-source-plugins.md).

When adding a **user-facing** runtime source, also wire `skippr connect source <kebab-name>` in `crates/skippr-cli` and map the public `kind` in `skippr_impl.rs` — see the **CLI (`skippr connect source`)** checklist in [API / SaaS source plugins](./api-saas-source-plugins.md#cli-skippr-connect-source).

## Single source of truth

Each runtime plugin crate declares its metadata in its own `Cargo.toml`:

```toml
[package.metadata.skippr-plugin]
kind = "DataSink"
plugin_name = "Athena"
```

That Cargo metadata is the source of truth for:

- plugin kind (`DataSource`, `DataSink`, `SchemaSink`)
- human-facing plugin name
- manifest filename and manifest name
- per-plugin package version
- build checksum inputs for semver clobber decisions
- capability descriptors

The release scripts no longer read committed JSON manifest templates.

## Catalog and publish flow

The runtime plugin release toolchain is:

1. `.github/scripts/runtime_plugin_catalog.py` reads Cargo metadata and computes the workspace catalog.
2. `.github/scripts/runtime_plugin_release_plan.py` compares the workspace catalog to the latest published manifest index.
3. platform build jobs produce the selected plugin binaries.
4. `.github/scripts/publish_runtime_plugins.py` generates versioned JSON manifests plus `latest/manifest-index.json`.

Manifest checksum split:

- `build_checksum` tracks source provenance for deciding whether reusing the current plugin semver requires a clobber
- `artifacts[*].sha256` verifies the published binary bytes after download

Published layout uses versioned paths like:

```text
plugins/<manifest-stem>/versions/<package-version>/<manifest-filename>
plugins/<manifest-stem>/versions/<package-version>/<target-triple>/<binary>
latest/manifest-index.json
```

## Discovery model

By default, the host resolves runtime plugins from:

```text
https://install.skippr.io/releases/runtime-plugins/latest/manifest-index.json
```

Resolution rules:

- default to the latest manifest index
- if a connector config includes a `version` field, pin only that plugin to that published version
- for maintainer and CI validation only, `USE_LOCAL_PLUGIN_CODE=1` can opt into generated local manifests before falling back to published discovery

Downloaded manifests and binaries are cached under `SKIPPR_RUNTIME_PLUGIN_DIR` when set, otherwise under `~/.skippr/runtime_plugins`.

## Local plugin code

Use the local plugin flow when a maintainer needs to validate the runtime plugin binaries built from the current checkout before publishing them:

```bash
cargo build -p skipprd
manifest_dir="$(python3 .github/scripts/local_runtime_plugins.py \
  --config path/to/skippr.yaml \
  --pipeline my_pipeline)"

export USE_LOCAL_PLUGIN_CODE=1
export SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR="$manifest_dir"
target/debug/skipprd --config path/to/skippr.yaml sync --pipeline my_pipeline --once
```

The helper parses the active Skippr config, builds only the referenced runtime plugin packages, and writes manifests under `.skippr/local-runtime-plugins/manifests` by default. The generated manifests point directly at `target/debug` or `target/release` binaries from the current commit.

Keep this as an internal maintainer mechanism. Do not add per-pipeline YAML fields for local plugin paths, and do not document `USE_LOCAL_PLUGIN_CODE` in public configuration docs.

Resolution behavior with `USE_LOCAL_PLUGIN_CODE=1`:

- matching local manifest found: validate kind/plugin/executable and run that binary
- local manifest directory missing or no matching manifest: fall back to published discovery
- matching manifest is malformed or points at a missing executable: fail loudly so CI does not accidentally test published code

## Runtime transport

Runtime plugins now connect back to the host over **TCP only**:

- one **control channel** for handshake, install frames, schema refresh, offset RPC, checkpoints, completion, and errors
- one **data channel** for Arrow payloads and source-emitted batch data
- a shared `RuntimeSessionHello` handshake on both sockets
- shared length-prefixed bincode framing from `crates/skippr-runtime-sdk`

There is no stdio fallback path.

## Host-owned offsets

The durable offsets database is backed by `sled`, but only the host process opens it.

Current behavior:

- runtime source plugins never open the durable offsets DB directly
- plugins may ask the host to validate resume state and load checkpoints
- plugins emit offset materialization hints and checkpoints back to the host
- the host writes offsets only after WAL-visible durability has been established

This keeps the offsets DB as a host-owned materialized view of committed WAL state instead of a second durability authority.

## Source, sink, and schema roles

- **Source plugins** read external systems, emit raw or prepared batches, emit checkpoints, and emit offset hints.
- **Sink plugins** receive Arrow streams from the host and perform replay-safe destination writes.
- **Schema sink plugins** synchronize destination schema/catalog state and must also tolerate replay.

The host may resend sink or schema work after reconnect, restart, or crash. Stable `compaction_id` values are the semantic idempotency key for that replay.

## Source execution contracts

Every source plugin must implement `DataSource::execution_contract()`. The contract declares both `--once` termination behavior and CDC behavior, so adding a new source without choosing these semantics fails at compile time.

CDC-capable source configs use `cdc_mode`:

| Value | Runtime behavior |
|---|---|
| `snapshot` | Bounded snapshot only. The source does not advertise a CDC capability. |
| `snapshot_then_cdc` | Initial snapshot, checkpoint, then native CDC stream. Later runs resume from checkpoints and skip the snapshot. |
| `cdc_only` | Native CDC stream only. No initial snapshot is performed. |

Use `SourceCdcMode` in plugin config structs and derive active CDC capability from `SourceExecutionContract`. Do not hand-roll separate config flags and capability branches.

## Config-level version pins

Plugin config entries support an optional `version` field. That gives maintainers a way to pin one runtime plugin while leaving the rest on latest:

```yaml
data_sources:
  s3_bike_hire:
    S3:
      version: 8.1.0
      s3_bucket: skippr-e2e-sample-data
      s3_prefix: bike-hire/
```

Use pins sparingly. The intended steady state is:

- host binary released on its own cadence
- plugins versioned per crate
- runtime discovery using latest by default

## Runtime sink process budget

`RUNTIME_SINK_CONNECTION_POOL_SIZE` is retained for compatibility, but its value is the global
total number of runtime data-sink child processes in the host, not a per-sink pool size.
`RUNTIME_SINK_POOL_TARGET` has the same global-total meaning. Both are capped at 16.

The host reserves at least one worker for every configured sink binding and divides remaining
workers fairly between primary and deadletter. If the configured total is below the binding count,
the host clamps it to that count and logs the clamp. Protocol v16 pools do not shrink after growth;
idle reaping and child multiplexing are deferred to protocol v17.

## Shared helper code

Small connector-agnostic helpers live under `plugins/shared/`.

Important rule: consuming plugin crates must still declare their own direct dependencies for whatever those shared modules use. Shared modules are source includes, not an implicit dependency bundle.

Those shared helper files are also part of the per-plugin `build_checksum`, so changing `plugins/shared/` is treated like changing plugin source for release planning.

In practice, sink crates that include shared CDC and parquet helpers usually need direct access to crates such as:

- `arrow`
- `bytes`
- `futures`
- `hex`
- `parquet`

## Guard rails

Use these checks after changing runtime plugin structure:

```bash
python3 .github/scripts/check_host_dependency_boundaries.py
cargo test -p skipprd --test runtime_plugin_global_guards -- --nocapture
cargo test -p skipprd --test runtime_source_plugin_guards -- --nocapture
cargo test -p skipprd --test runtime_host_contracts -- --nocapture
cargo check --workspace
```

Those checks catch most regressions around deleted legacy paths, host/plugin coupling, replay semantics, and missing per-crate dependencies.
