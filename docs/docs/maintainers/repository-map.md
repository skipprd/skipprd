# Repository Map

This repository is a Cargo workspace with a single host binary and many separately versioned runtime plugin crates.

## Workspace layout

| Path | Responsibility |
|---|---|
| `python/` | PyO3 `skipprd` module. Maturin crate wrapping `Session`. Own semver in `pyproject.toml` / `python/Cargo.toml`, not the skipprd git tag. CI always builds and tests the wheel via `scripts/test-python.sh` on Skippr Cloud runners. PyPI publish is GitHub OIDC, only when that Python semver is new. |
| `crates/skippr-core/` | Shared core logic used by the host and plugin crates. |
| `crates/skippr-query-ballista/` | `FlightSqlExec` physical node and SkipprPhysicalCodec for Ballista 53. |
| `src/cluster/` | Clustered WAL replica, lease scheduler, gossip, promotion, WAL head picker. |
| `src/query_flight/` | Arrow Flight SQL 58.3 on `flight_addr`, live WAL selector, in-process Ballista. |
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

- `skipprd` owns orchestration, WAL indexing, schema state, and the durable offsets database.
- Runtime plugins are child processes discovered from published manifests or explicit local manifest overrides.
- Runtime plugins talk to the host over TCP control/data channels, and runtime source plugins never open the durable offsets database directly.
- Plugin crates own connector-specific code; the host must not take direct dependencies on connector implementation crates.

## Current plugin source of truth

The plugin catalog is Cargo-driven:

- each runtime plugin crate defines `[package.metadata.skippr-plugin]` in its own `Cargo.toml`
- `.github/scripts/runtime_plugin_catalog.py` reads Cargo metadata and computes per-plugin build checksums from the plugin crate plus `plugins/shared/`
- generated manifests and binaries are published under versioned paths on `install.skippr.io`

There are no committed runtime manifest templates, and there is no aggregate `skippr-plugin-runtime-link` crate anymore.

## Multi-node HLA

Normative clustered-query design:

- [hla-distributed-query-iceberg-catalog.md](hla-distributed-query-iceberg-catalog.md) — architecture index
- [hla-flight-sql-ballista.md](hla-flight-sql-ballista.md) — Arrow Flight SQL 58.3 and Ballista 53 (shipped in tree)
- [hla-implementation-wbs.md](hla-implementation-wbs.md) — work units including WU-7.3 / WU-7.5
- [hla-observability-otel-console.md](hla-observability-otel-console.md) — OTel lakehouse + SQL/UDFs
- [hla-observability-implementation-wbs.md](hla-observability-implementation-wbs.md) — observability work units

## Useful guard rails

When working on architecture boundaries, the highest-value checks are:

- `python3 .github/scripts/check_host_dependency_boundaries.py`
- `cargo test -p skipprd --test runtime_plugin_global_guards -- --nocapture`
- `cargo test -p skipprd --test runtime_source_plugin_guards -- --nocapture`
- `cargo test -p skipprd --test runtime_host_contracts -- --nocapture`
- `cargo check --workspace --exclude skipprd-python`
- `python3 .github/scripts/test_python_bindings_ci.py`
- `./scripts/test-python.sh`

These catch most regressions around host/plugin coupling, deleted legacy paths, and source-plugin protocol usage.
