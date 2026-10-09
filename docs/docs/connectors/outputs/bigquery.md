---
title: BigQuery
description: Load Skipprd pipelines into a BigQuery dataset. Tables are created on the first sync.
---

# BigQuery

Use BigQuery when that is the warehouse analysts already open. Skipprd writes rows into `project.dataset` and creates tables as the schema appears.

## Before you begin

- A GCP project and dataset.
- A service-account JSON with `bigquery.tables.create`, `bigquery.tables.update`, and `bigquery.tables.updateData` on that dataset (plus `bigquery.jobs.create`).
- Store the key path in the environment, not in git.

```bash
export GOOGLE_APPLICATION_CREDENTIALS="/path/to/sa.json"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkBigquery, Pipeline

cfg = Config.discover()
warehouse = cfg.data_sink(
    "warehouse",
    DataSinkBigquery(
        project="acme-analytics",
        dataset="raw",
        location="EU",
        credentials_path="${GOOGLE_APPLICATION_CREDENTIALS}",
    ),
)
cfg.pipeline("events", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=warehouse))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink bigquery \
  --pipeline events \
  --name warehouse \
  --project acme-analytics \
  --dataset raw \
  --location EU \
  --credentials-path '${GOOGLE_APPLICATION_CREDENTIALS}'
```

```yaml [YAML]
data_sinks:
  warehouse:
    Bigquery:
      project: acme-analytics
      dataset: raw
      location: EU
      credentials_path: ${GOOGLE_APPLICATION_CREDENTIALS}
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `project` | string | Required | GCP project id |
| `dataset` | string | Required | Dataset id |
| `location` | string | Not set | Dataset location (`US`, `EU`, or a region) |
| `credentials_path` | path | Not set | Service-account JSON. Falls back to Application Default Credentials. |
| `max_concurrency` | integer | Not set | Query/model only |
| `discovery_cache_ttl_secs` | integer | Not set | Query/model only |

## How data lands

Each namespace becomes a table in the dataset. Retries are exactly once. Schema changes add columns. See [Quickstart: BigQuery](/getting-started/quickstart-bigquery).

## Troubleshooting

| Symptom | Fix |
|---|---|
| `403 Access Denied` | Grant the service account dataset permissions |
| Dataset not found | Create the dataset in `location`, or let an admin create it |
| Credentials not found | Export `GOOGLE_APPLICATION_CREDENTIALS` in this shell |

## Next steps

- [Quickstart: BigQuery](/getting-started/quickstart-bigquery)
- [Exactly-once delivery](/concepts/exactly-once)
