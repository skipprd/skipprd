# Skippr Developer & Maintainer Docs

Skippr is a Rust ingestion host plus a published runtime plugin ecosystem. The host binary owns pipeline orchestration, WAL lifecycle, schema state, and durable offsets; connector crates are built and published separately, then resolved from the runtime plugin registry at `install.skippr.io`.

This docs site is now organized for repository maintainers first. Operator-facing install and quick-start material is still here, but the high-signal entry points are the maintainer guides and architecture pages.

## Start here

- [Repository Map](maintainers/repository-map.md) for the current workspace layout and ownership boundaries
- [Local Development](maintainers/local-development.md) for the commands that match CI and the fastest local verification loops
- [Runtime Plugins](maintainers/runtime-plugins.md) for the Cargo-driven plugin catalog, manifest generation, cache layout, and version pinning model
- [Runtime Plugin Contract](maintainers/runtime-plugin-contract.md) for the WAL-first durability rules, TCP runtime session shape, and crash matrix
- [Runtime E2E Harness](maintainers/runtime-e2e-harness.md) for the local and CI AWS acceptance flows
- [Release Workflow](maintainers/release-workflow.md) for the tag-driven host and runtime plugin publishing pipeline

## Current operating model

- **Single host binary**: `skippr-el` runs discovery, sync, query, WAL replay, compaction, and schema coordination.
- **Published runtime plugins**: source, sink, and schema plugins are resolved from the latest published manifest index by default, with optional per-plugin version pins.
- **Host-owned offsets**: the durable `sled` offsets database lives in the host process only; runtime source plugins read resume state from the host over the TCP runtime protocol.
- **Generated plugin manifests**: plugin metadata comes from each plugin crate's `Cargo.toml`, not committed manifest templates.
- **CI-backed crash recovery**: chaos-mode and deadletter scenarios are exercised through the shared runtime e2e harness and release workflow.

## High-signal commands

```bash
cargo check --workspace
cargo test -p skippr -- --nocapture
python3 -m unittest discover -s .github/scripts -p 'test_*.py'
mkdocs build -f docs/mkdocs.yml --strict
```

## Operator docs

If you are using Skippr rather than maintaining it, start with [Installation](getting-started/install.md) and the [Quick Start](getting-started/quickstart.md).

## License

Skippr is licensed under the [Elastic License 2.0 (ELv2)](license.md).
