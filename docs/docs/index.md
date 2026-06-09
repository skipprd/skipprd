# Skippr Docs

Skippr is a data pipeline CLI and runtime for configuring sources, sinks, warehouses, models, and vector ingestion from one `skippr.yml` file.

Most users start with the `skippr` product CLI. The lighter `skipprd` binary runs the same engine commands against the same `skippr.yml` and is useful for engine-only deployments, Lambda images, and runtime plugin testing.

## Start here

- [Installation](getting-started/install.md) — install `skippr` and optionally `skipprd`
- [Quick Start](getting-started/quickstart.md) — configure S3 to Athena with `skippr.yml`
- [skippr.yml reference](configuration/skippr-yml.md) — the canonical config shape
- [Warehouses](configuration/warehouses.md) — query/model/catalog providers, separate from ingest sinks
- [CLI overview](cli/overview.md) — which commands are shared and which are `skippr`-only

## Current operating model

- **One config**: `skippr.yml` is the product and engine contract.
- **Two binaries**: `skippr` is the full product CLI; `skipprd` is the lightweight engine runtime.
- **Shared engine commands**: `discover`, `sync`, and engine query/schema commands read the same pipeline config through either binary.
- **Runtime plugins**: source, sink, and schema plugins are resolved from `install.skippr.io` on demand.
- **Separate warehouse providers**: `warehouses` drive query/model/catalog workflows and do not replace `data_sinks`.

## Maintainer docs

Repository internals, runtime plugin contracts, CI harnesses, and release workflow live under [Maintainers](maintainers/repository-map.md).

## License

Skippr is licensed under the [Elastic License 2.0 (ELv2)](license.md).
