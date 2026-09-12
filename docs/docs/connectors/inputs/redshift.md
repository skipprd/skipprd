# Redshift Input

Reads data from Amazon Redshift using the Data API.

## How it works

1. Submits SQL statements via the Redshift Data API.
2. Polls for statement completion, then fetches results.
3. Rows are converted to JSON.
4. Namespace convention: `redshift.{database}.{table_name}`.

## Configuration

```yaml
data_sources:
  source:
    Redshift:
      cluster_identifier: my-cluster
      database: analytics
      db_user: admin
      tables: ["events"]
      region: us-east-1
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `cluster_identifier` | | Redshift cluster identifier |
| `workgroup_name` | | Serverless workgroup (alternative to cluster) |
| `database` | *(required)* | Database name |
| `db_user` | | Database user (for cluster mode) |
| `tables` | | List of tables to read |
| `query` | | Custom SQL query |
| `region` | | AWS region |
| `format` | `json` | Data format |

## Authentication

Authentication uses the AWS default credential chain.

- `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`
- IAM roles, instance profiles, or task roles
- AWS SSO or shared config profiles

## Troubleshooting

| Symptom | Fix |
|---|---|
| authentication or API errors | Verify the AWS credential chain, region, and cluster or workgroup identifiers. |
| query returns no rows | Check the selected tables and confirm the database user can read them. |
