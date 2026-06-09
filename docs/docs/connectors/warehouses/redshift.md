# Redshift Warehouse

Query and model data in Amazon Redshift.

Ingest may use the [Redshift data sink](../outputs/redshift.md).

## Configuration

```yaml
warehouses:
  primary:
    kind: redshift
    database: analytics
    schema: public
    cluster_identifier: my-cluster
    region: us-east-1
    db_user: admin
    staging_s3_bucket: my-redshift-staging
    staging_s3_prefix: staging/
    iam_role_arn: arn:aws:iam::123456789012:role/RedshiftCopyRole
```

| Field | Description |
| --- | --- |
| `kind` | `redshift` |
| `database` | Database name |
| `schema` | Default schema |
| `cluster_identifier` | Provisioned cluster ID |
| `workgroup_name` | Serverless workgroup name (alternative to cluster) |
| `db_user` | Database user for IAM or standard auth |
| `region` | AWS region |
| `staging_s3_bucket` | S3 bucket for bulk staging |
| `staging_s3_prefix` | Key prefix within staging bucket |
| `iam_role_arn` | IAM role for `COPY` from S3 |

## Related

- [Redshift ingest](../outputs/redshift.md)
- [Warehouses overview](../../configuration/warehouses.md)
