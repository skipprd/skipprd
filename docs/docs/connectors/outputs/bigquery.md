# BigQuery Output

Writes data to Google BigQuery using the BigQuery REST API. Automatically creates datasets and tables, and evolves schema by adding columns as new fields are discovered.

## How it works

1. Authenticates via a GCP service account JSON key file (JWT-based OAuth2).
2. Ensures the target dataset exists (`CREATE SCHEMA IF NOT EXISTS`).
3. Ensures the target table exists with the correct columns (`CREATE TABLE IF NOT EXISTS`), and adds any missing columns (`ALTER TABLE ADD COLUMN IF NOT EXISTS`).
4. Converts Arrow record batches to SQL `INSERT INTO ... VALUES` statements.
5. Executes inserts via the BigQuery Jobs API, polling for completion on long-running queries.

## Configuration

```bash
DATA_OUTPUT_PLUGIN_NAME=Bigquery
BIGQUERY_PROJECT=my-gcp-project
BIGQUERY_DATASET=my_dataset
BIGQUERY_LOCATION=US
GOOGLE_APPLICATION_CREDENTIALS=/path/to/service-account.json
```

Or via YAML pipeline config:

```yaml
data_sinks:
  sink:
    Bigquery:
      project: "my-gcp-project"
      dataset: "my_dataset"
      location: "US"
      credentials_path: "/path/to/service-account.json"
      max_concurrency: 4
      discovery_cache_ttl_secs: 300
```

`max_concurrency` and `discovery_cache_ttl_secs` are optional query/model keys; ingest ignores them.

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `project` / `BIGQUERY_PROJECT` | *(required)* | GCP project ID |
| `dataset` / `BIGQUERY_DATASET` | *(required)* | BigQuery dataset name |
| `location` / `BIGQUERY_LOCATION` | | Dataset location (e.g. `US`, `EU`) |
| `credentials_path` / `GOOGLE_APPLICATION_CREDENTIALS` | *(required)* | Path to service account JSON key |

## GCP permissions required

The service account needs:

- `bigquery.datasets.create`, `bigquery.datasets.get`
- `bigquery.tables.create`, `bigquery.tables.get`, `bigquery.tables.update`
- `bigquery.tables.updateData`
- `bigquery.jobs.create`

## Type mapping

| Arrow Type | BigQuery Type |
|---|---|
| Boolean | BOOL |
| Int8 / Int16 / Int32 / Int64 / UInt* | INT64 |
| Float16 / Float32 / Float64 | FLOAT64 |
| Date32 / Date64 | DATE |
| Timestamp | TIMESTAMP |
| Utf8 / LargeUtf8 | STRING |
| Other | STRING |

## Namespace convention

Table names are derived from the pipeline namespace with dots replaced by underscores and lowercased:

```
namespace: "app.events" -> table: "app_events"
```

Tables are fully qualified as `` `project.dataset.table` ``.

## Authentication

Authentication uses a GCP service account key file.

| Variable | Description |
|---|---|
| `GOOGLE_APPLICATION_CREDENTIALS` | Path to a GCP service account JSON key file |

### Setting up a service account

1. In the GCP Console, go to **IAM & Admin > Service Accounts**.
2. Create a service account with the **BigQuery Data Editor** and **BigQuery Job User** roles.
3. Create a JSON key and download it.
4. Set the environment variable:

```bash
export GOOGLE_APPLICATION_CREDENTIALS="/path/to/service-account.json"
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| `Could not automatically determine credentials` | Verify `GOOGLE_APPLICATION_CREDENTIALS` points to a valid JSON key file |
| `Access Denied: Dataset` | Check the service account has the required roles on the project |
| `Not found: Dataset` | The dataset will be created automatically; ensure the service account has `bigquery.datasets.create` |
