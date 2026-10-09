---
title: "Quickstart: BigQuery"
description: Load sample JSON from S3 into a BigQuery table with a service account, using skippr.yml, skipprd discover, and skipprd sync.
---

# Quickstart: BigQuery

In about fifteen minutes you will load public sample JSON from S3 into a BigQuery table and query it.

Skipprd infers the schema, creates the BigQuery dataset and table for you, and inserts the rows with BigQuery jobs. You provide a Google Cloud project and a service account.

## Before you begin

You need:

- **Skipprd installed.** Check with `skipprd --version`, or `python -c "import skippr"` if you use Python. See [Install](/getting-started/install).
- **A Google Cloud project** with the BigQuery API enabled, and permission to create service accounts and grant roles in it.
- **The `gcloud` and `bq` command-line tools**, signed in to that project. You can do the same steps in the Google Cloud console instead.
- **AWS credentials** that can read `s3://skippr-public-sample-data/bike-hire/`. Any AWS account works.

Export your credentials and project ID, then create an empty working directory. Skipprd reads `skippr.yml` from the current directory, so run every command from there.

```bash
export AWS_ACCESS_KEY_ID="your-access-key-id"
export AWS_SECRET_ACCESS_KEY="your-secret-access-key"
export AWS_DEFAULT_REGION="us-east-1"
export GCP_PROJECT="my-gcp-project"

mkdir skippr-bigquery && cd skippr-bigquery
```

## 1. Create a service account

Skipprd authenticates to BigQuery with a service account key file. The service account needs **BigQuery Data Editor** (create datasets and tables, write rows) and **BigQuery Job User** (run the jobs that insert the rows).

```bash
gcloud iam service-accounts create skippr-loader --project "$GCP_PROJECT"

gcloud projects add-iam-policy-binding "$GCP_PROJECT" \
  --member "serviceAccount:skippr-loader@$GCP_PROJECT.iam.gserviceaccount.com" \
  --role roles/bigquery.dataEditor

gcloud projects add-iam-policy-binding "$GCP_PROJECT" \
  --member "serviceAccount:skippr-loader@$GCP_PROJECT.iam.gserviceaccount.com" \
  --role roles/bigquery.jobUser

gcloud iam service-accounts keys create skippr-loader.json \
  --iam-account "skippr-loader@$GCP_PROJECT.iam.gserviceaccount.com"

export GOOGLE_APPLICATION_CREDENTIALS="$PWD/skippr-loader.json"
```

Keep `skippr-loader.json` out of version control. It grants write access to BigQuery in your project.

## 2. Describe the pipeline

Each tab writes the same `skippr.yml`. Replace `my-gcp-project` with your project ID.

::: code-group

```python [Python]
from skippr import Config, DataSinkBigquery, DataSourceS3, LocalStorage, Pipeline

cfg = Config().workspace("quickstart").storage(LocalStorage())
sample = cfg.data_source(
    "sample",
    DataSourceS3(s3_bucket="skippr-public-sample-data", s3_prefix="bike-hire"),
)
warehouse = cfg.data_sink(
    "warehouse",
    DataSinkBigquery(
        project="my-gcp-project",
        dataset="skippr_quickstart",
        location="US",
        credentials_path="${GOOGLE_APPLICATION_CREDENTIALS}",
    ),
)
cfg.pipeline("bikehire", Pipeline(data_source=sample, data_sink=warehouse))
cfg.save("skippr.yml")
```

```bash [CLI]
skipprd --workspace quickstart --storage-mode local connect data-source s3 \
  --pipeline bikehire \
  --name sample \
  --s3-bucket skippr-public-sample-data \
  --s3-prefix bike-hire

skipprd connect data-sink bigquery \
  --pipeline bikehire \
  --name warehouse \
  --project my-gcp-project \
  --dataset skippr_quickstart \
  --location US \
  --credentials-path '${GOOGLE_APPLICATION_CREDENTIALS}'
```

```yaml [YAML]
skippr:
  workspace: quickstart
  skipprd_el_storage_mode: local

pipelines:
  bikehire:
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
      project: my-gcp-project
      dataset: skippr_quickstart
      location: US
      credentials_path: ${GOOGLE_APPLICATION_CREDENTIALS}
```

:::

What the settings mean:

- `credentials_path` is required. Skipprd reads the key only from this setting; it does not pick up `GOOGLE_APPLICATION_CREDENTIALS` on its own. Writing `${GOOGLE_APPLICATION_CREDENTIALS}` reads the path from the environment when the pipeline starts. In the CLI, single quotes stop your shell from expanding it early.
- `location` is where Skipprd creates the dataset if it does not exist. If the dataset already exists, use its location.
- `LocalStorage()` (`skipprd_el_storage_mode: local`) keeps Skipprd's own state, such as the discovered schema and how far it has read, in `./data`. That is fine for a trial; for production, see [State store](/configuration/skippr-store).

## 3. Discover the schema

::: code-group

```python [Python]
import skippr

s = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
s.discover()
```

```bash [CLI]
skipprd discover --pipeline bikehire
skipprd schema --pipeline bikehire
```

:::

`discover` samples the JSON and records each field's name and type. It does not touch BigQuery. `skipprd schema` prints the fields, which become your table's columns.

## 4. Sync

::: code-group

```python [Python]
import skippr

s = skippr.Session(skippr.Config.discover().get_pipeline("bikehire"))
s.sync(once=True)
```

```bash [CLI]
skipprd sync --pipeline bikehire --once
```

:::

Sync reads the source and writes each batch to Skipprd's write-ahead log (WAL) first. A crash retries that committed batch; BigQuery applies it once. It then creates the `skippr_quickstart` dataset and the `bikehire` table if they do not exist, and inserts the rows with BigQuery jobs. When new fields appear later, Skipprd adds the columns before loading.

`--once` (`once=True`) runs a single pass and exits. Without it, sync keeps running and checks the source for new data on a schedule.

## Check it worked

Count the rows and look at a sample:

```bash
bq query --use_legacy_sql=false \
  "SELECT COUNT(*) AS row_count FROM \`$GCP_PROJECT.skippr_quickstart.bikehire\`"

bq query --use_legacy_sql=false \
  "SELECT * FROM \`$GCP_PROJECT.skippr_quickstart.bikehire\` LIMIT 10"
```

You should see a non-zero `row_count`, and columns that match the fields `skipprd schema` printed in step 3. The table is named after the pipeline, in lower case. You can run the same SQL in the BigQuery console.

## Troubleshooting

- **`BigQuery requires a non-empty credentials_path in plugin config`** — `credentials_path` is missing, or `GOOGLE_APPLICATION_CREDENTIALS` was empty when the pipeline started. Export it in the shell running Skipprd, or add it to a `.env` file next to `skippr.yml`.
- **`skippr.yml references ${GOOGLE_APPLICATION_CREDENTIALS} ... but that environment variable is not set`** — same cause. Export the variable and rerun.
- **Skipprd cannot read the key file** — the path is wrong or the file is not readable by the user running Skipprd. Check with `ls -l "$GOOGLE_APPLICATION_CREDENTIALS"`.
- **`Access Denied` on the project, dataset, or a job** — the service account is missing a role. Grant **BigQuery Data Editor** and **BigQuery Job User** as in step 1.
- **A location error, or `Not found: Dataset`** — the dataset exists in a different location from `location`. Set `location` to the dataset's location, or use a new dataset name.
- **`AccessDenied` reading the sample bucket** — your AWS credentials are missing or lack `s3:GetObject` and `s3:ListBucket`. Run `aws sts get-caller-identity` to see which principal Skipprd uses.

For more detail, rerun the failing command with `--log debug`, or see [Troubleshooting](/operations/troubleshooting).

## Next steps

- [BigQuery destination](/connectors/outputs/bigquery) — every option and the type mapping.
- [S3 source](/connectors/inputs/s3) — point the pipeline at your own bucket.
- [Schema discovery and evolution](/concepts/schema) — how types are inferred and how new fields are added.
- [How Skipprd works](/concepts/how-it-works) — what happens between source and destination.
