
# Run Local Build

```bash
AWS_PROFILE=skippr \
S3_BUCKET=skpr-sample-data-output \
S3_PREFIX=skpr-sample-data/test \
DATA_SOURCE_BATCH_SIZE=6 \
DATA_OUTPUT_S3_BUCKET=skpr-sample-data-output \
DATA_OUTPUT_S3_PREFIX=test123 \
GLUE_DATABASE_NAME=test123_db \
ATHENA_WORKGROUP_NAME=test123 \
PIPELINE_NAME=cubevents \
PIPELINE_ID=69 \
SKIPPR_API_TOKEN=kfI5drp8VhsFgeRfPYBRvMoto7aChpb95UuoxNNC \
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
DATA_SOURCE_FLATTEN_EVENTS=yes
DATA_OUTPUT_TIME_BUCKET
GLUE_DATABASE_NAME
ATHENA_WORKGROUP_NAME



# Demo

```bash
AWS_PROFILE=cloudcycle \
S3_BUCKET=production-datastorage-stack-cubeevents9ad2ae37-ots21z45kn3g \
S3_PREFIX=inventory/production-datastorage-stack-cubeevents9ad2ae37-ots21z45kn3g \
DATA_SOURCE_BATCH_SIZE_BYTES=2048000 \
DATA_SOURCE_EVENT_TYPE_FIELDS='detail-type' \
DATA_OUTPUT_PARTITION_BY_FIELDS=detail.imei,detail.truck_registration \
DATA_OUTPUT_S3_BUCKET=production-datalake-stac-datalakeskipprbucket4a91-db0ekzd8fkz0 \
DATA_OUTPUT_S3_PREFIX=test123 \
DATA_OUTPUT_TIME_BUCKET=day \
DATA_OUTPUT_TIME_FIELDS=time \
GLUE_DATABASE_NAME=test123_db \
ATHENA_WORKGROUP_NAME=test123 \
PIPELINE_NAME=cubevents \
PIPELINE_ID=69 \
SKIPPR_API_TOKEN=B8ib6S3wa9nSq5wAwxkO9dceUIg04d4uTYUHBDg \
DATA_DIR=./data \
cargo run sync
```

```bash
AWS_PROFILE=skippr_old \
S3_BUCKET=skpr-sample-data-output \
S3_PREFIX=skpr-sample-data/smail-files \
DATA_SOURCE_BATCH_SIZE_BYTES=2048000 \
DATA_OUTPUT_S3_BUCKET=production-datalake-stac-datalakeskipprbucket4a91-db0ekzd8fkz0 \
DATA_OUTPUT_S3_PREFIX=bikehire \
GLUE_DATABASE_NAME=bikehire \
ATHENA_WORKGROUP_NAME=bikehire \
PIPELINE_NAME=bikehire \
PIPELINE_ID=69 \
SKIPPR_API_TOKEN=B8ib6S3wa9nSq5wAwxkO9dceUIg04d4uTYUHBDg \
DATA_DIR=./data \
cargo run sync
```


# Performance Profiling

```bash
sudo AWS_PROFILE=skippr_old \
S3_BUCKET=skpr-sample-data-output \
S3_PREFIX=skpr-sample-data/smail-files \
DATA_SOURCE_BATCH_SIZE_BYTES=2048000 \
DATA_OUTPUT_S3_BUCKET=production-datalake-stac-datalakeskipprbucket4a91-db0ekzd8fkz0 \
DATA_OUTPUT_S3_PREFIX=bikehire \
GLUE_DATABASE_NAME=bikehire \
ATHENA_WORKGROUP_NAME=bikehire \
PIPELINE_NAME=bikehire \
PIPELINE_ID=69 \
SKIPPR_API_TOKEN=B8ib6S3wa9nSq5wAwxkO9dceUIg04d4uTYUHBDg \
DATA_DIR=./data \
cargo flamegraph --dev -- sync
```

```bash
AWS_PROFILE=cloudcycle \
DATA_SOURCE_PLUGIN_NAME=s3 \
S3_BUCKET=production-datastorage-stack-cubeevents9ad2ae37-ots21z45kn3g \
S3_PREFIX=/ \
DATA_SOURCE_BATCH_SIZE_BYTES=2048000 \
DATA_OUTPUT_S3_BUCKET=production-datalake-stac-datalakeskipprbucket4a91-db0ekzd8fkz0 \
DATA_OUTPUT_S3_PREFIX=test123 \
GLUE_DATABASE_NAME=test123_db \
ATHENA_WORKGROUP_NAME=test123 \
PIPELINE_NAME=cubevents7 \
SKIPPR_API_TOKEN=B8ib6S3wa9nSq5wAwxkO9dceUIg04d4uTYUHBDg \
APP_ENV=dev \
RUST_BACKTRACE=1 \
DATA_DIR=./data \
cargo run sync
```

