---
title: "Quick Start: BigQuery"
description: Load a source into BigQuery with Skipprd discover and Skipprd sync using a GCP service account.
---

# Quick Start: BigQuery

Land a source in BigQuery. Skipprd creates the dataset and tables when they are missing, then inserts rows through the Jobs API.

## Prerequisites

- `skipprd` on `PATH` ([Install](install.md))
- A GCP project and a service account JSON key with BigQuery job and data permissions
- AWS credentials if the source is S3

```bash
export GOOGLE_APPLICATION_CREDENTIALS="/path/to/service-account.json"
export BIGQUERY_PROJECT="my-gcp-project"
export BIGQUERY_DATASET="my_dataset"
export AWS_ACCESS_KEY_ID="your-key"
export AWS_SECRET_ACCESS_KEY="your-secret"
export AWS_DEFAULT_REGION="us-east-1"
```

## skippr.yml

```yaml
skippr:
  workspace: quickstart
  skippr_s3_bucket: your-state-bucket

pipelines:
  files:
    data_source: data_sources.sample
    data_sink: data_sinks.warehouse

data_sources:
  sample:
    S3:
      s3_bucket: skippr-public-sample-data
      s3_prefix: bike-hire

data_sinks:
  warehouse:
    Bigquery:
      project: "my-gcp-project"
      dataset: "my_dataset"
      location: "US"
      credentials_path: "/path/to/service-account.json"
```

## Discover, schema, sync

```bash
skipprd discover --pipeline files --log
skipprd schema --pipeline files
skipprd sync --pipeline files --once --log
```

After sync, query the landed table in BigQuery.

See [BigQuery sink](/connectors/outputs/bigquery) for IAM roles.

## Troubleshooting

- **Could not load credentials** — `credentials_path` and `GOOGLE_APPLICATION_CREDENTIALS` must point at a service account JSON file the Skipprd process can read.
- **Access Denied on dataset** — the account needs `bigquery.datasets.create` (or an existing dataset) plus table create/update and job create.
- **Location mismatch** — `location` must match the dataset location (`US`, `EU`, or a region).

## Next

- [Pipeline flow](how-it-works.md)
- [Schema discovery](/concepts/schema)
