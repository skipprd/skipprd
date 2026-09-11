# Skippr Docs

Skippr is a data pipeline CLI and runtime for configuring sources, sinks, models, and vector ingestion from one `skippr.yml` file.

The engine binary is `skipprd`. Data Engineer is `sde`. Cloud is `skippr`. `skipprd discover` and `skipprd sync` invoke the skipprd runtime against the same file. `skipprd` remains the engine binary for Lambda images, plugin testing, and maintainer debugging.

## Start here

- [Installation](getting-started/install.md) — install `skippr`
- [Quick Start](getting-started/quickstart.md) — configure S3 to Athena with `skippr.yml`
- [skippr.yml reference](configuration/skippr-yml.md) — the canonical config shape
- [CLI overview](cli/overview.md) — product commands on `skippr`

## Current operating model

- **One config**: `skippr.yml` is the product and engine contract. Plugin entries live under `data_sources` and `data_sinks`.
- **One product CLI**: `skippr` is the public interface. `skipprd` is the runtime implementation detail.
- **Shared engine YAML**: discover, sync, model, and query compile the same sinks. There is no `warehouses:` dialect.
- **Runtime plugins**: source, sink, and schema plugins are resolved from `install.skippr.io` on demand.

## Maintainer docs

Repository internals, runtime plugin contracts, CI harnesses, and release workflow live under [Maintainers](maintainers/repository-map.md).

## License

Skippr is licensed under the [Elastic License 2.0 (ELv2)](license.md).
