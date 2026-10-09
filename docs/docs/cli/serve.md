---
title: skipprd serve
description: Expose your Iceberg tables over Iceberg REST and read-only Flight SQL so Spark, Trino, PyIceberg, and dbt can read them.
---

# skipprd serve

`skipprd serve` starts two endpoints for the Iceberg tables your pipelines write: an Iceberg REST catalog for Iceberg clients (PyIceberg, Spark, Trino) and a read-only Flight SQL endpoint for SQL clients. Use it when tools other than Skipprd need to read your lake. It serves pipelines whose destination is SkipprLake, AthenaIceberg, or DuckDB, all sharing one Iceberg catalog. Pipelines with other destinations are not served. Both endpoints require a bearer token.

## Usage

```bash
export SKIPPRLAKE_TOKEN='replace-with-a-long-random-token'
```

```bash
skipprd serve [--rest-bind <addr>] [--flight-bind <addr>] [--token-env <VAR>] \
  [--tls-cert <pem> --tls-key <pem>] [--ready-file <path>] [--config <path>]
```

There is no Python equivalent; Python clients connect to the running server, for example with PyIceberg's REST catalog.

## Options

| Flag | Default | Description |
|---|---|---|
| `--rest-bind <ADDR>` | `127.0.0.1:8181` | Address for the Iceberg REST catalog. Port `0` picks a free port. |
| `--flight-bind <ADDR>` | `127.0.0.1:8815` | Address for the Flight SQL endpoint. Port `0` picks a free port. |
| `--token-env <VAR>` | `SKIPPRLAKE_TOKEN` | Name of the environment variable holding the bearer token. The variable must be set and non-empty. Clients send the token as a bearer token. |
| `--tls-cert <PATH>` | None | TLS certificate (PEM). Must be used with `--tls-key`. Required when either address is not a loopback address. |
| `--tls-key <PATH>` | None | TLS private key (PEM). Must be used with `--tls-cert`. |
| `--ready-file <PATH>` | None | Once both endpoints are listening, write their URLs to this file as JSON. Useful for scripts and tests that wait for the server. |
| `--config <PATH>` | `./skippr.yml` | Config file to read. |
| `--log [LEVEL]` | Off | Print logs to stderr, including the listening addresses. |

See [CLI overview](/cli/overview) for the other global flags. `serve` does not run with `--wal-storage clustered`.

### What clients see

- **Iceberg REST** serves the catalog: namespaces, tables, and their metadata. Clients can create and update their own namespaces and tables through it. Namespaces that Skipprd pipelines write into are read-only.
- **Flight SQL** is read-only. Tables are named by their Iceberg identity, `<namespace>.<table>`, for example `bronze.rides`. Listing tables returns every Iceberg namespace and table. Flight SQL reads committed Iceberg data; for live WAL data use [`skipprd query`](/cli/query).

## Examples

### Serve locally

```bash
export SKIPPRLAKE_TOKEN='replace-with-a-long-random-token'
skipprd --config skippr.yml serve --ready-file ready.json
```

When both endpoints are up, `ready.json` contains:

```json
{
  "rest": "http://127.0.0.1:8181",
  "flight": "grpc://127.0.0.1:8815"
}
```

Stop the server with Ctrl-C.

### Serve on a network with TLS

Any non-loopback address requires TLS. With TLS on, the ready file uses `https` and `grpc+tls`.

```bash
export SKIPPRLAKE_TOKEN='replace-with-a-long-random-token'
skipprd serve \
  --rest-bind 0.0.0.0:8181 \
  --flight-bind 0.0.0.0:8815 \
  --tls-cert /etc/skipprd/tls/server.crt \
  --tls-key /etc/skipprd/tls/server.key \
  --ready-file /run/skipprd/ready.json
```

### Use a different token variable

```bash
export LAKE_READ_TOKEN='replace-with-a-long-random-token'
skipprd serve --token-env LAKE_READ_TOKEN
```

### Pick free ports in tests

```bash
skipprd serve --rest-bind 127.0.0.1:0 --flight-bind 127.0.0.1:0 --ready-file ready.json
```

The ready file records the ports that were actually bound.

## Troubleshooting

`serve` prints the error and exits `1` if it cannot start. It exits `0` after Ctrl-C.

| Message | Cause | Fix |
|---|---|---|
| `SKIPPRLAKE_TOKEN must be set to a non-empty bearer token` | The token variable is missing or empty. | `export SKIPPRLAKE_TOKEN=...`, or point `--token-env` at the variable you use. |
| `non-loopback bind ... requires --tls-cert and --tls-key` | You bound to a network address without TLS. | Add `--tls-cert` and `--tls-key`, or bind to `127.0.0.1`. |
| `--tls-cert and --tls-key are required together` | Only one of the two was given. | Pass both. |
| `... at least one Iceberg sink ...` | No pipeline writes to SkipprLake, AthenaIceberg, or DuckDB. | Add an Iceberg destination to a pipeline. |
| `... one Iceberg catalog ...` | Pipelines write to different Iceberg catalogs, for example one SkipprLake and one AthenaIceberg. | Serve one catalog per `serve` process, each with its own `skippr.yml`. |
| `ingest namespace '<name>' equals a pipeline name` | A destination's Iceberg namespace has the same name as a pipeline, which makes table names ambiguous. | Rename the Iceberg namespace (for example `table_namespace: bronze`) or the pipeline. |
| `Address already in use` | Another process is using the port. | Choose another port, or `0` for a free one. |

## Next steps

- [Datalake](/concepts/datalake)
- [SkipprLake destination](/connectors/outputs/skipprlake)
- [skipprd query](/cli/query)
