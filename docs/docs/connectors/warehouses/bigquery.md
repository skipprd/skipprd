# BigQuery Warehouse

Query and model data in Google BigQuery.

Ingest may use the [BigQuery data sink](../outputs/bigquery.md).

## Configuration

```yaml
warehouses:
  primary:
    kind: bigquery
    project: my-gcp-project
    dataset: analytics
    location: EU
```

| Field | Description |
| --- | --- |
| `kind` | `bigquery` |
| `project` | GCP project ID |
| `dataset` | Default dataset |
| `location` | BigQuery location (for example `US`, `EU`) |
| `max_concurrency` | Parallel query cap |
| `discovery_cache_ttl_secs` | Catalog discovery cache TTL |

## Credentials

Use Application Default Credentials or a service account key path configured in your environment.

## Related

- [BigQuery ingest](../outputs/bigquery.md)
- [Warehouses overview](../../configuration/warehouses.md)
