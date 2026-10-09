---
title: Redshift data source
description: Copy Amazon Redshift tables or query results into your destination with Skipprd through the Redshift Data API and AWS credentials.
---

# Redshift

The Redshift source reads tables, or the result of one SQL query, from an Amazon Redshift provisioned cluster or Redshift Serverless workgroup through the Redshift Data API. It authenticates with AWS credentials, so no database password or open database port is needed. It is a one-time full load: each table is copied once, and later sync runs do not load new or changed rows.

## Before you begin

- **Network access.** The machine running Skipprd must reach the Redshift Data API endpoint for your region over HTTPS. It does not connect to the cluster's database port.
- **AWS credentials.** Skipprd uses the standard AWS credential chain: environment variables, a shared profile, SSO, or an instance, task, or pod role.
- **An IAM policy.** The caller needs the Data API actions plus permission to obtain database credentials:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "redshift-data:ExecuteStatement",
        "redshift-data:DescribeStatement",
        "redshift-data:GetStatementResult"
      ],
      "Resource": "*"
    },
    {
      "Effect": "Allow",
      "Action": [
        "redshift:GetClusterCredentials",
        "redshift:GetClusterCredentialsWithIAM",
        "redshift-serverless:GetCredentials"
      ],
      "Resource": "*"
    }
  ]
}
```

Keep the second statement's action for your setup: `redshift:GetClusterCredentials` for a cluster with `db_user`, `redshift:GetClusterCredentialsWithIAM` for a cluster without `db_user`, or `redshift-serverless:GetCredentials` for a workgroup. Scope `Resource` to your cluster or workgroup ARN in production.

- **Database grants.** The database user needs `SELECT` on what you read:

```sql
GRANT USAGE ON SCHEMA public TO skippr;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO skippr;
```

## Configure

Export AWS credentials if you are not using a role:

```bash
export AWS_ACCESS_KEY_ID="AKIA..."
export AWS_SECRET_ACCESS_KEY="..."
export AWS_DEFAULT_REGION="us-east-1"
```

This example reads two tables from a provisioned cluster:

::: code-group

```python [Python]
from skippr import Config, DataSourceRedshift, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "redshift",
    DataSourceRedshift(
        cluster_identifier="analytics-cluster",
        database="analytics",
        db_user="skippr",
        region="us-east-1",
        tables=["public.events", "public.users"],
    ),
)
pipe = cfg.pipeline("redshift", Pipeline(data_source=src))
cfg.save()
```

```bash [CLI]
skipprd connect data-source redshift \
  --pipeline redshift \
  --name redshift \
  --cluster-identifier analytics-cluster \
  --database analytics \
  --db-user skippr \
  --region us-east-1 \
  --tables public.events \
  --tables public.users
```

```yaml [YAML]
pipelines:
  redshift:
    data_source: data_sources.redshift

data_sources:
  redshift:
    Redshift:
      cluster_identifier: analytics-cluster
      database: analytics
      db_user: skippr
      region: us-east-1
      tables: ["public.events", "public.users"]
```

:::

For Redshift Serverless, replace `cluster_identifier` and `db_user` with `workgroup_name`:

```yaml
data_sources:
  redshift:
    Redshift:
      workgroup_name: analytics-wg
      database: analytics
      region: us-east-1
      tables: ["public.events"]
```

Add a destination to the pipeline (see the [connector catalog](/connectors/)), then discover the schema and run one sync pass:

::: code-group

```python [Python]
from skippr import Session

session = Session(pipe)
session.discover()
session.sync(once=True)
```

```bash [CLI]
skipprd discover --pipeline redshift --log
skipprd sync --pipeline redshift --once --log
```

:::

## Options

Set either `cluster_identifier` or `workgroup_name`, and either `tables` or `query`. If you set both `tables` and `query`, Skipprd runs `query` and ignores `tables`.

| Key | Type | Default | Description |
|---|---|---|---|
| `database` | string | Required | Database to query. |
| `cluster_identifier` | string | none | Provisioned cluster identifier. |
| `workgroup_name` | string | none | Redshift Serverless workgroup name. |
| `db_user` | string | none | Database user for a provisioned cluster. Without it, the Data API maps your IAM identity to a database user. |
| `region` | string | from the AWS credential chain | AWS region of the cluster or workgroup. |
| `tables` | list of strings | none | Tables to read. Each entry is used as written in `SELECT * FROM <entry>`. |
| `query` | string | none | One SQL `SELECT` to run instead of `tables`. |
| `format` | string | `json` | Leave unset. Rows are always sent as JSON. |
| `batch_size_bytes` | integer | none | Not used by this source. |
| `batch_size_seconds` | integer | none | Not used by this source. |

## What gets synced

**Tables.** Each entry in `tables`, or the single result of `query`. Skipprd submits each statement, waits for it to finish, and pages through the whole result before writing it, so size the machine for your largest table.

**Table names.** Each table lands in a destination table named `redshift_<database>_<table>`: `public.events` in `analytics` becomes `redshift_analytics_public_events`. A `query` lands in `redshift_<database>_query`. Skipprd lowercases names and replaces characters other than letters, digits, and `_` with `_`.

**Types.**

| Data API value | Arrives as |
|---|---|
| Integer types | number |
| `REAL`, `DOUBLE PRECISION` | number |
| `BOOLEAN` | boolean |
| `VARCHAR`, `CHAR`, `DECIMAL`, dates and timestamps | string, as the Data API returns them |
| Binary (`VARBYTE`) | hexadecimal string |
| Types the Data API returns in other forms, such as arrays | `null` |

Run `skipprd discover` to infer column types; see [Schema discovery and evolution](/concepts/schema).

**Incremental behaviour.** Skipprd records each table (or the query) as loaded once its rows are durably written. Later sync runs still execute the query on Redshift, but its rows are not loaded again. To reload, run the source under a new pipeline name, which starts with fresh progress.

**Deletes.** Not captured.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `AccessDeniedException` or `not authorized to perform: redshift-data:...` | The IAM policy is missing an action. | Add the actions in [Before you begin](#before-you-begin). |
| `not authorized to perform: redshift:GetClusterCredentials` | Using `db_user` without credential permission. | Allow `redshift:GetClusterCredentials` for that user, or remove `db_user` and allow `GetClusterCredentialsWithIAM`. |
| `ValidationException` naming the cluster or workgroup | Wrong identifier, or the wrong region. | Check `cluster_identifier` or `workgroup_name`, and set `region`. |
| `Redshift statement FAILED: ... permission denied for relation` | The database user lacks `SELECT`. | Grant `USAGE` on the schema and `SELECT` on the table. |
| `Redshift statement FAILED: ... does not exist` | The table name is wrong. | Schema-qualify the entry in `tables`. |
| `Redshift: must specify either 'tables' or 'query'` | Neither is set. | Add `tables` or `query`. |
| New rows never arrive | Each table loads once. | Run the source under a new pipeline name to take a fresh copy. |

## Next steps

- [Connector catalog](/connectors/) — pick a destination
- [Redshift destination](/connectors/outputs/redshift) — land data in Redshift
- [skipprd sync](/cli/sync) — run once or on a schedule
