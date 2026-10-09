---
title: Redshift destination
description: Load Skipprd pipelines into Amazon Redshift using S3 staging and a COPY.
---

# Redshift

Skipprd stages Parquet on S3 and COPYs it into Redshift. You need a cluster or Serverless workgroup, a staging bucket, and an IAM role Redshift can assume to read that bucket.

## Before you begin

- Cluster identifier **or** workgroup name, plus the database and a user (`db_user`).
- Staging bucket and prefix.
- `iam_role_arn` that Redshift uses for `COPY`.

```bash
export AWS_DEFAULT_REGION="us-east-1"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSinkRedshift, Pipeline

cfg = Config.discover()
rs = cfg.data_sink(
    "warehouse",
    DataSinkRedshift(
        database="analytics",
        cluster_identifier="acme-warehouse",
        db_user="skippr",
        table="events",
        region="us-east-1",
        staging_s3_bucket="acme-skipprd-stage",
        staging_s3_prefix="redshift/",
        iam_role_arn="arn:aws:iam::123456789012:role/RedshiftCopy",
        schema="public",
    ),
)
cfg.pipeline("events", Pipeline(data_source=cfg.get_data_source("sample"), data_sink=rs))
cfg.save()
```

```bash [CLI]
skipprd connect data-sink redshift \
  --pipeline events \
  --name warehouse \
  --database analytics \
  --cluster-identifier acme-warehouse \
  --db-user skippr \
  --table events \
  --region us-east-1 \
  --staging-s3-bucket acme-skipprd-stage \
  --staging-s3-prefix redshift/ \
  --iam-role-arn arn:aws:iam::123456789012:role/RedshiftCopy \
  --schema public
```

```yaml [YAML]
data_sinks:
  warehouse:
    Redshift:
      database: analytics
      cluster_identifier: acme-warehouse
      db_user: skippr
      table: events
      region: us-east-1
      staging_s3_bucket: acme-skipprd-stage
      staging_s3_prefix: redshift/
      iam_role_arn: arn:aws:iam::123456789012:role/RedshiftCopy
      schema: public
```

:::

For Serverless, set `workgroup_name` instead of `cluster_identifier`.

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `database` | string | Required | Database |
| `cluster_identifier` | string | Not set | Provisioned cluster. Set this or `workgroup_name`. |
| `workgroup_name` | string | Not set | Serverless workgroup |
| `db_user` | string | Required | Database user |
| `table` | string | Required | Table |
| `region` | string | Required | AWS region |
| `staging_s3_bucket` | string | Required | Staging bucket |
| `staging_s3_prefix` | string | Required | Staging prefix |
| `iam_role_arn` | string | Required | Role Redshift assumes for COPY |
| `schema` | string | Not set | Schema (default `public`) |

## How data lands

Each batch is uploaded to the staging prefix, then COPY loads it. Retries are exactly once. Staging objects are not a public dataset — treat the bucket as pipeline state.

## Troubleshooting

| Symptom | Fix |
|---|---|
| COPY access denied | Attach S3 read on the staging prefix to `iam_role_arn` |
| Cluster not found | Check `cluster_identifier` vs `workgroup_name` and `region` |
| User cannot connect | Use Data API / IAM auth the cluster already allows for `db_user` |

## Next steps

- [Redshift source](/connectors/inputs/redshift)
- [Exactly-once delivery](/concepts/exactly-once)
