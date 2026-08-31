# DynamoDB Input

Reads items from an Amazon DynamoDB table using a paginated `Scan`.

## Supported formats

- Row-based JSON (each item serialized as a JSON object)

## How it works

1. Connects to DynamoDB in the configured AWS region (or the default credential chain region).
2. Scans the target table with pagination until all items are read.
3. Parallel scan segments are auto-tuned based on CPU count.
4. Batches are ingested through the standard WAL pipeline.
5. Lake namespace is the pipeline name (same as S3). AWS table identity stays in offset keys only.

## Configuration

```bash
DATA_SOURCE_PLUGIN_NAME=Dynamodb
DYNAMODB_TABLE_NAME=my-table
AWS_DEFAULT_REGION=us-east-1
```

Or via YAML pipeline config:

```yaml
data_sources:
  source:
    Dynamodb:
      table_name: "my-table"
      region: "us-east-1"
```

## Configuration variables

| Variable | Default | Description |
|---|---|---|
| `DYNAMODB_TABLE_NAME` | *(required)* | DynamoDB table to scan |
| `AWS_DEFAULT_REGION` | | AWS region for the DynamoDB client |
| `table_name` | | Table name (YAML; can be set via env) |
| `region` | | Optional region override (YAML) |

## AWS credentials

DynamoDB access uses the standard AWS credential chain (same as S3).

## Namespace convention

DynamoDB is one AWS table per pipeline. The lake table uses the **pipeline name**, the same default as S3. Offset and stream checkpoints stay keyed by the AWS table name (`dynamodb:` / `dynamodb-stream:`), so renaming the DynamoDB table in AWS does not change the warehouse table unless you also rename the pipeline.

Optional `transform.namespace_fields` can still fan out records from field values. Use `cdc.default` for the business-key contract; do not key `cdc.namespaces` on the AWS table name.

## Type mapping

| DynamoDB Attribute | Skippr Type |
|---|---|
| `S` (String) | String |
| `N` (Number) | Number |
| `BOOL` | Boolean |
| `NULL` | Null |
| `L` (List) | Array |
| `M` (Map) | Object |
| `B` (Binary) | Base64 String |
| `SS` (String Set) | String Set |
| `NS` (Number Set) | Number Set |
