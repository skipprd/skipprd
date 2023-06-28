
# Run Local Build

```bash
AWS_PROFILE=skippr-test \
DATA_SOURCE_PLUGIN_NAME=s3_inventory \
S3_BUCKET=skippr-e2e-sample-data \
S3_PREFIX=skippr-e2e-sample-data/inventory-bike-hire \
DATA_SOURCE_BATCH_SIZE_BYTES=2048000 \
DATA_OUTPUT_S3_BUCKET=skippr-e2e-sample-data-output \
DATA_OUTPUT_S3_PREFIX=bikehire \
GLUE_DATABASE_NAME=test123_db \
ATHENA_WORKGROUP_NAME=test123 \
PIPELINE_NAME=s3_inventory \
SKIPPR_API_TOKEN=9kLhlh43vc0NbNc6QW3mZ9lF9NzuQz23 \
APP_ENV=dev \
DATA_DIR=./data \
cargo run sync
```

# Build and Run Release

```bash
SDKROOT=$(xcrun -sdk macosx12.3 --show-sdk-path) \
MACOSX_DEPLOYMENT_TARGET=$(xcrun -sdk macosx12.3 --show-sdk-platform-version) \
cargo build --release --target=x86_64-apple-darwin
```

```bash
SDKROOT=$(xcrun -sdk macosx12.3 --show-sdk-path) \
MACOSX_DEPLOYMENT_TARGET=$(xcrun -sdk macosx12.3 --show-sdk-platform-version) \
cargo build --target x86_64-unknown-linux-gnu --release
```

### Run Release

```bash
AWS_PROFILE=skippr \                                                                                                               
S3_BUCKET=skpr-sample-data-output \
S3_PREFIX=skpr-sample-data/test \
PIPELINE_ID=69 \
SKIPPR_API_TOKEN=kfI5drp8VhsFgeRfPYBRvMoto7aChpb95UuoxNNC \
DATA_DIR=./data \
./target/x86_64-apple-darwin/release/skipprd sync
```

### Put release into demo project

```
chmod 755 target/x86_64-apple-darwin/release/skipprd && cp target/x86_64-apple-darwin/release/skipprd ~/Downloads/skipprcli/skipprd
```


# Configuration



DATA_SOURCE_EVENT_TYPE_FIELDS="field_1,field_2.sub_field_2a"
TRANSFORM_FLATTEN_EVENTS=yes
DATA_OUTPUT_TIME_BUCKET
GLUE_DATABASE_NAME
ATHENA_WORKGROUP_NAME

DATA_OUTPUT_TIME_FIELDS=system_datetime_posix_utc_seconds




# Demo

```bash
AWS_PROFILE=cloudcycle \
DATA_SOURCE_S3_BUCKET=production-datastorage-stack-cubeevents9ad2ae37-ots21z45kn3g \
DATA_SOURCE_S3_PREFIX=/2021 \
DATA_SOURCE_BATCH_SIZE_BYTES=2048000 \
DATA_SOURCE_EVENT_TYPE_FIELDS='detail-type' \
DATA_OUTPUT_S3_BUCKET=production-datalake-stac-datalakeskipprbucket4a91-db0ekzd8fkz0 \
DATA_OUTPUT_S3_PREFIX=test123 \
DATA_OUTPUT_TIME_BUCKET=day \
DATA_OUTPUT_TIME_FIELDS=time \
GLUE_DATABASE_NAME=test123_db \
ATHENA_WORKGROUP_NAME=test123 \
PIPELINE_NAME=cubevents \
SKIPPR_API_TOKEN=9kLhlh43vc0NbNc6QW3mZ9lF9NzuQz23 \
DATA_DIR=./data \
RUST_BACKTRACE=1 \
cargo run sync
```

```bash
AWS_PROFILE=skippr-test \
DATA_SOURCE_PLUGIN_NAME=s3 \
DATA_SOURCE_S3_BUCKET=skippr-e2e-sample-data \
DATA_SOURCE_S3_PREFIX=bike-hire \
DATA_SOURCE_BATCH_SIZE_BYTES=10048000 \
TRANSFORM_BATCH_PARTITION_FIELDS=event_type \
TRANSFORM_BATCH_TIME_FIELDS=event_date \
TRANSFORM_BATCH_TIME_UNIT=day \
DATA_OUTPUT_S3_BUCKET=skippr-e2e-sample-data-output \
DATA_OUTPUT_S3_PREFIX=test \
SCHEMA_OUTPUT_GLUE_DATABASE_NAME=bikehire \
DATA_OUTPUT_ATHENA_WORKGROUP_NAME=bikehire \
PIPELINE_NAME=bikehire \
WORKSPACE_NAME=skpr9 \
SKIPPR_API_TOKEN=9kLhlh43vc0NbNc6QW3mZ9lF9NzuQz23 \
DATA_DIR=./data \
APP_ENV=dev \
TRANSFORM_FLATTEN_EVENTS=yes \
cargo run sync
```

```bash
cat foo.txt | AWS_PROFILE=skippr-test \
DATA_SOURCE_PLUGIN_NAME=stdin \
DATA_SOURCE_BATCH_SIZE_BYTES=1 \
DATA_SOURCE_BATCH_SIZE_SECONDS=1 \
TRANSFORM_NAMESPACE_FIELDS=event_type \
DATA_OUTPUT_S3_BUCKET=skippr-e2e-sample-data-output \
DATA_OUTPUT_S3_PREFIX=test \
SCHEMA_OUTPUT_GLUE_DATABASE_NAME=bikehire \
DATA_OUTPUT_ATHENA_WORKGROUP_NAME=bikehire \
PIPELINE_NAME=bikehire \
WORKSPACE_NAME=foo1 \
SKIPPR_API_TOKEN=9kLhlh43vc0NbNc6QW3mZ9lF9NzuQz23 \
DATA_DIR=./data \
APP_ENV=dev \
TRANSFORM_FLATTEN_EVENTS=yes \
cargo run sync
```


# Performance Profiling

```bash
sudo AWS_PROFILE=skippr_old \
DATA_SOURCE_PLUGIN_NAME=s3 \
DATA_SOURCE_S3_BUCKET=skpr-sample-data \
DATA_SOURCE_S3_PREFIX=small-files \
DATA_SOURCE_BATCH_SIZE_BYTES=2048000 \
DATA_OUTPUT_S3_BUCKET=production-datalake-stac-datalakeskipprbucket4a91-db0ekzd8fkz0 \
DATA_OUTPUT_S3_PREFIX=bikehire \
GLUE_DATABASE_NAME=bikehire \
ATHENA_WORKGROUP_NAME=bikehire \
SKIPPR_API_TOKEN=B8ib6S3wa9nSq5wAwxkO9dceUIg04d4uTYUHBDg \
PIPELINE_NAME=bikehire \
APP_ENV=dev \
TRANSFORM_FLATTEN_EVENTS=yes \
DATA_DIR=./data \
cargo flamegraph --dev -- sync
```

production-datastorage-stack-cubeevents9ad2ae37-ots21z45kn3g

```bash
AWS_PROFILE=cloudcycle-dev \
DATA_SOURCE_PLUGIN_NAME=s3 \
DATA_SOURCE_S3_BUCKET=dev-datastorage-stack-cubeevents9ad2ae37-ufazj652azlb \
DATA_SOURCE_S3_PREFIX=/2022/ \
DATA_SOURCE_BATCH_SIZE_BYTES=2048000 \
BUFFER_THRESHOLD_BYTES=2000000 \
BUFFER_THRESHOLD_SECONDS=10 \
TRANSFORM_NAMESPACE_FIELDS='detail-type' \
TRANSFORM_FLATTEN_EVENTS=yes \
DATA_OUTPUT_PLUGIN_NAME=athena \
DATA_OUTPUT_S3_BUCKET=cloudcycle-datalake-dev \
DATA_OUTPUT_S3_PREFIX=test123 \
SCHEMA_OUTPUT_PLUGIN_NAME=glue \
SCHEMA_OUTPUT_GLUE_DATABASE_NAME=test123_db \
DATA_OUTPUT_ATHENA_WORKGROUP_NAME=test123 \
WORKSPACE_NAME=sadf \
PIPELINE_NAME=cubeevents \
SKIPPR_API_TOKEN=9kLhlh43vc0NbNc6QW3mZ9lF9NzuQz23 \
APP_ENV=dev \
DATA_DIR=./data \
cargo run sync
```

production-datastorage-stac-rawdevicejson568138dc-1djhnp70ebsko

```bash
AWS_PROFILE=cloudcycle \
DATA_SOURCE_PLUGIN_NAME=s3 \
S3_BUCKET=production-datastorage-stac-rawdevicejson568138dc-1djhnp70ebsko \
S3_PREFIX=data/2022/08/10 \
DATA_SOURCE_BATCH_SIZE_BYTES=2048000 \
BUFFER_THRESHOLD_BYTES=2000000 \
BUFFER_THRESHOLD_SECONDS=300 \
TRANSFORM_FLATTEN_EVENTS=yes \
DATA_OUTPUT_TIME_FIELDS=system.datetime_posix_utc_seconds \
DATA_OUTPUT_TIME_BUCKET=day \
DATA_OUTPUT_PLUGIN_NAME=athena \
DATA_OUTPUT_S3_BUCKET=production-datalake-stac-datalakeskipprbucket4a91-db0ekzd8fkz0 \
DATA_OUTPUT_S3_PREFIX=test123 \
SCHEMA_OUTPUT_PLUGIN_NAME=glue \
SCHEMA_OUTPUT_GLUE_DATABASE_NAME=test123_db \
DATA_OUTPUT_ATHENA_WORKGROUP_NAME=test123 \
PIPELINE_NAME=devicedata \
SKIPPR_API_TOKEN=9kLhlh43vc0NbNc6QW3mZ9lF9NzuQz23 \
APP_ENV=dev \
DATA_DIR=./data \
cargo run sync
```


docker run --platform=linux/x86_64 \
-e AWS_DEFAULT_REGION=eu-west-1 \
-e AWS_ACCESS_KEY_ID=$AWS_ACCESS_KEY_ID \
-e AWS_SECRET_ACCESS_KEY=$AWS_SECRET_ACCESS_KEY \
-e DATA_SOURCE_PLUGIN_NAME=s3_inventory \
-e S3_BUCKET=SOUR_BUCKET_NAME \
-e S3_PREFIX=/example/inventory-dir \
-e DATA_SOURCE_BATCH_SIZE_BYTES=2048000 \
-e DATA_OUTPUT_S3_BUCKET=example\
-e DATA_OUTPUT_S3_PREFIX=example \
-e GLUE_DATABASE_NAME=test123 \
-e ATHENA_WORKGROUP_NAME=test123 \
-e PIPELINE_NAME=test123 \
-e SKIPPR_API_TOKEN=PgFeheJ4C7AdWkgz7nTS98XfowTTx3qq \
-e DATA_DIR=./ \
-e APP_ENV=test \
-v `pwd`/data:/data \
skippr/skipprd:v3.1.0
