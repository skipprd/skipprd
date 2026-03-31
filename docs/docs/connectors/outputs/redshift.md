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
```

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
