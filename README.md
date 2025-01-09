# Skippr

### What is Skippr?

Skippr is a tool for data ingestion and transformation. It is designed to ingest data from a source and transform it into a destination datalake/warehouse.

### Project Structure

- `src/` - Source code for the Skippr CLI and library
- `data/` - Data directory for the Skippr CLI
- `target/` - Build output for the Skippr CLI

### Building the Project

#### Local Development Build
```bash
cargo run sync
```

#### Release Builds

For MacOS:
```bash
SDKROOT=$(xcrun -sdk macosx12.3 --show-sdk-path) \
MACOSX_DEPLOYMENT_TARGET=$(xcrun -sdk macosx12.3 --show-sdk-platform-version) \
cargo build --release --target=x86_64-apple-darwin
```

For Linux:
```bash
cargo build --target x86_64-unknown-linux-gnu --release
```

### Configuration

Skippr is configured through environment variables. Here are the key configuration options:

#### Data Source Configuration
- `DATA_SOURCE_PLUGIN_NAME` - Source plugin to use (e.g. 's3', 's3_inventory', 'stdin')
- `DATA_SOURCE_S3_BUCKET` - Source S3 bucket
- `DATA_SOURCE_S3_PREFIX` - Source S3 prefix
- `DATA_SOURCE_BATCH_SIZE_BYTES` - Batch size in bytes
- `DATA_SOURCE_EVENT_TYPE_FIELDS` - Fields to use for event type

#### Transform Configuration  
- `TRANSFORM_FLATTEN_EVENTS` - Whether to flatten nested JSON events
- `TRANSFORM_NAMESPACE_FIELDS` - Fields to use for namespacing
- `TRANSFORM_BATCH_TIME_FIELDS` - Fields to use for time-based partitioning
- `TRANSFORM_BATCH_TIME_UNIT` - Time unit for partitioning (day/year)

#### Output Configuration
- `DATA_OUTPUT_PLUGIN_NAME` - Output plugin to use (e.g. 'athena')
- `DATA_OUTPUT_S3_BUCKET` - Destination S3 bucket
- `DATA_OUTPUT_S3_PREFIX` - Destination S3 prefix
- `SCHEMA_OUTPUT_PLUGIN_NAME` - Schema output plugin (e.g. 'glue')
- `SCHEMA_OUTPUT_GLUE_DATABASE_NAME` - Glue database name
- `DATA_OUTPUT_ATHENA_WORKGROUP_NAME` - Athena workgroup name

### Example Usage

Basic S3 to Athena pipeline:
```bash
AWS_PROFILE=skippr-test \
DATA_SOURCE_PLUGIN_NAME=s3 \
DATA_SOURCE_S3_BUCKET=skippr-e2e-sample-data \
DATA_SOURCE_S3_PREFIX=bike-hire \
DATA_SOURCE_BATCH_SIZE_BYTES=10048000 \
DATA_OUTPUT_PLUGIN_NAME=athena \
DATA_OUTPUT_S3_BUCKET=skippr-e2e-sample-data-output \
DATA_OUTPUT_S3_PREFIX=bikehire \
SCHEMA_OUTPUT_PLUGIN_NAME=glue \
SCHEMA_OUTPUT_GLUE_DATABASE_NAME=bikehire \
DATA_OUTPUT_ATHENA_WORKGROUP_NAME=bikehire \
PIPELINE_NAME=bikehire \
SKIPPR_API_TOKEN=your_api_token \
DATA_DIR=./data \
APP_ENV=dev \
cargo run sync
```

### Docker Usage

```bash
docker run --platform=linux/x86_64 \
-e AWS_DEFAULT_REGION=eu-west-1 \
-e AWS_ACCESS_KEY_ID=$AWS_ACCESS_KEY_ID \
-e AWS_SECRET_ACCESS_KEY=$AWS_SECRET_ACCESS_KEY \
-e DATA_SOURCE_PLUGIN_NAME=s3_inventory \
-e DATA_SOURCE_S3_BUCKET=source-bucket \
-e DATA_SOURCE_S3_PREFIX=/example/inventory-dir \
-e DATA_OUTPUT_S3_BUCKET=dest-bucket \
-e DATA_OUTPUT_S3_PREFIX=example \
-e SCHEMA_OUTPUT_GLUE_DATABASE_NAME=test123 \
-e DATA_OUTPUT_ATHENA_WORKGROUP_NAME=test123 \
-e PIPELINE_NAME=test123 \
-e SKIPPR_API_TOKEN=your_api_token \
-e DATA_DIR=./ \
-e APP_ENV=test \
-v `pwd`/data:/data \
skippr/skipprd:v3.1.0
```
