# serve

Expose Iceberg REST and read-only Flight SQL for **one physical Iceberg catalog** in the config. Iceberg sinks are SkipprLake, AthenaIceberg, and Duckdb. Hive Athena and other non-Iceberg sinks are not served.

REST is for Iceberg clients (PyIceberg, Spark, Trino). Flight SQL reads the
same lake as Iceberg `namespace.table` (`bronze.shop`, `shop_gold.fct_shop`).
`GetTables` lists every Iceberg namespace and table. `pipeline.namespace` remains
the local Iceberg ∪ WAL ingest alias. Ingest namespaces are read-only over REST.

## Usage

```bash
export SKIPPRLAKE_TOKEN=replace-me
skipprd --config skippr.yml serve \
  --rest-bind 127.0.0.1:8181 \
  --flight-bind 127.0.0.1:8815 \
  --ready-file ready.json
```

`--token-env` names the environment variable that holds the bearer token (default `SKIPPRLAKE_TOKEN`). The variable must be set and non-empty.

Loopback binds may be plaintext. Non-loopback binds require `--tls-cert` and `--tls-key`.

`--ready-file` writes `{"rest":"http://127.0.0.1:8181","flight":"grpc://127.0.0.1:8815"}` once both listeners are up. TLS uses `https` and `grpc+tls`. Bind port `0` is valid; the ready file records the bound addresses.

All Iceberg sinks in the config must share one physical catalog. Mixed SkipprLake and AthenaIceberg catalogs fail closed.

## Flags

| Flag | Required | Description |
|---|---|---|
| `--rest-bind` | No | Iceberg REST bind address. Default `127.0.0.1:8181`. |
| `--flight-bind` | No | Flight SQL bind address. Default `127.0.0.1:8815`. |
| `--token-env` | No | Name of the env var that holds the bearer token. Default `SKIPPRLAKE_TOKEN`. |
| `--tls-cert` | With `--tls-key` | TLS cert PEM. Required with `--tls-key` for non-loopback binds. |
| `--tls-key` | With `--tls-cert` | TLS key PEM. |
| `--ready-file` | No | Write REST and Flight URLs once both listeners are up. |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Process exited after SIGINT |
| 1 | Missing token, mixed catalogs, bind, or TLS error |
