# Redshift Output

Writes data to Amazon Redshift using S3 staging + COPY (preferred) or direct INSERT via the Redshift Data API.

## How it works

### S3 Staging mode (when `staging_s3_bucket` is set)

1. Serializes Arrow batches to Parquet.
2. Uploads Parquet to the staging S3 bucket.
3. Executes `COPY INTO` via the Redshift Data API.
4. Requires an IAM role ARN with Redshift COPY permissions.

### INSERT mode (fallback when no S3 bucket)

1. Converts Arrow batches to SQL `INSERT INTO ... VALUES` statements.
2. Executes via the Redshift Data API with polling for completion.

## Configuration

```yaml
data_sinks:
  sink:
    Redshift:
      database: my_warehouse
      cluster_identifier: my-cluster
      staging_s3_bucket: my-staging-bucket
      staging_s3_prefix: skippr-staging/
      iam_role_arn: "arn:aws:iam::123456789012:role/RedshiftCopyRole"
      table: events
      schema: public
```

`schema` is an optional query/model key; ingest uses `table` for writes.

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `database` | | Redshift database name |
| `cluster_identifier` | | Redshift cluster identifier |
| `workgroup_name` | | Redshift Serverless workgroup (alternative to cluster) |
| `db_user` | | Database user for Data API |
| `table` | (from namespace) | Target table name |
| `region` | (AWS default) | AWS region |
| `staging_s3_bucket` | | S3 bucket for COPY staging |
| `staging_s3_prefix` | `skippr-staging` | S3 prefix for staged files |
| `iam_role_arn` | | IAM role for Redshift COPY |
| `format` | `parquet` | Data format |

## Authentication

Authentication uses the AWS default credential chain.

- `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`
- IAM roles, instance profiles, or task roles
- AWS SSO or shared config profiles

## Troubleshooting

| Symptom | Fix |
|---|---|
| COPY or staging errors | Verify the staging S3 bucket, IAM role ARN, region, and Redshift cluster or workgroup settings. |
| permission denied | Check the Redshift database user, schema permissions, and the IAM role Redshift uses to read from S3. |
