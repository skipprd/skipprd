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
