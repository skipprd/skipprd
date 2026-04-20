# Repository Map

This repository is a Cargo workspace with a single host binary and many separately versioned runtime plugin crates.

## Workspace layout

| Path | Responsibility |
|---|---|
| `src/` | Host-side Skippr application code. CLI entrypoints, ingest orchestration, WAL handling, runtime plugin discovery/hosting, and schema state live here. |
| `crates/skippr-core/` | Shared core logic used by the host and plugin crates. |
| `crates/skippr-runtime-sdk/` | Runtime plugin SDK, shared TCP protocol/wire helpers, and host bridge code for source/sink/schema plugins. |
| `plugins/data_source/*/` | Runtime source plugin crates. Each crate owns its connector implementation and Cargo metadata. |
| `plugins/data_sink/*/` | Runtime sink plugin crates. Each crate owns its connector implementation and Cargo metadata. |
| `plugins/schema_sink/*/` | Runtime schema sink plugin crates. |
| `plugins/shared/` | Small connector-agnostic helper modules reused by multiple plugin crates. These helpers do not replace direct crate dependencies in the consuming plugins. |
| `tests/` | Host integration tests plus runtime plugin guard tests. |
| `.github/scripts/` | CI and release scripts, including runtime plugin catalog generation, release planning, publishing, and the runtime e2e harness. |
| `.github/actions/e2e/` | Scenario configs and wrappers used by the runtime e2e jobs. |
| `soda/` | Soda assertions used by full runtime e2e validations. |
| `docs/` | MkDocs config, source markdown, and generated site output. |

## Host vs plugin boundary

The important architectural split is:

- `skippr-el` owns orchestration, WAL indexing, schema state, and the durable offsets database.
- Runtime plugins are child processes discovered from published manifests or explicit local manifest overrides.
- Runtime plugins talk to the host over TCP control/data channels, and runtime source plugins never open the durable offsets database directly.
- Plugin crates own connector-specific code; the host must not take direct dependencies on connector implementation crates.

## Current plugin source of truth

The plugin catalog is Cargo-driven:

- each runtime plugin crate defines `[package.metadata.skippr-plugin]` in its own `Cargo.toml`
- `.github/scripts/runtime_plugin_catalog.py` reads Cargo metadata and computes per-plugin checksums
- generated manifests and binaries are published under versioned paths on `install.skippr.io`

There are no committed runtime manifest templates, and there is no aggregate `skippr-plugin-runtime-link` crate anymore.

## Useful guard rails

When working on architecture boundaries, the highest-value checks are:

- `python3 .github/scripts/check_host_dependency_boundaries.py`
- `cargo test -p skippr --test runtime_plugin_global_guards -- --nocapture`
- `cargo test -p skippr --test runtime_source_plugin_guards -- --nocapture`
- `cargo test -p skippr --test runtime_host_contracts --test runtime_file_csv_to_file -- --nocapture`
- `cargo check --workspace`

These catch most regressions around host/plugin coupling, deleted legacy paths, and source-plugin protocol usage.
