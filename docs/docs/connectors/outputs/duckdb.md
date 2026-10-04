# Duckdb Output

Writes compacted batches to Apache Iceberg tables on the local filesystem (`file://`). Query those tables with `skipprd query` (Iceberg ∪ WAL) or DuckDB `iceberg_scan`. DuckDB sees persisted Iceberg only — not the skipprd WAL.

Pair with the [Duckdb schema sink](../schema_sinks/duckdb.md).

Skippr Data Engineer does not model DuckDB as a warehouse.

## Configuration

```yaml
data_sinks:
  local:
    Duckdb:
      warehouse: file:///Users/me/lake
      table_namespace: bronze
    schema_sink: schema_sinks.local_schema
```

Table location is `{warehouse}/{table_namespace}/{table_name}`. Each Duckdb data sink must own a unique warehouse plus table namespace. Surrounding whitespace and a trailing slash on warehouse do not make a second namespace.

Warehouse is `file://` only.

## Read with DuckDB

Do not enable `unsafe_enable_version_guessing`. The sink writes Hadoop `v{N}.metadata.json` plus `version-hint.text` so DuckDB 1.5 `iceberg_scan` opens the current snapshot:

```sql
INSTALL iceberg;
LOAD iceberg;
SELECT count(*) FROM iceberg_scan('/Users/me/lake/bronze/source');
```

Pass the table directory (`{warehouse}/{table_namespace}/{table}`), not a metadata file.

## Supported write policies

| Policy | Supported |
| --- | --- |
| `append` | Yes |
| `merge_by_key` | No |
| `replace_partition` | No |
| `replace_table` | Yes |

CDC sources that need merge-by-key are rejected at config time. See [Source landing semantics](../../concepts/source-landing-semantics.md).

## Pipeline wiring

```yaml
pipelines:
  reports:
    data_source: data_sources.files
    data_sink: data_sinks.local
```

Pairing is `data_sinks.<name>.schema_sink`, not a pipeline-level `schema_sink` field.

## Related

- [Duckdb schema sink](../schema_sinks/duckdb.md)
- [Output destination](../../configuration/output.md)
