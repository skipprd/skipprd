# S3 Bucket Output Plugin

Writes ingested data to an S3 bucket.

##### Data Formats

- json
- avro
- parquet

See: [Skippr serialisation formats](/formats)

### Config

```bash
DATA_OUTPUT_PLUGIN_NAME: "s3 bucket"
DATA_OUTPUT_S3_BUCKET: bucket-name
DATA_OUTPUT_AWS_REGION: eu-west-2
DATA_OUTPUT_AWS_ACCESS_ID: [SECRET]
DATA_OUTPUT_AWS_SECRET_KEY: [SECRET]
DATA_OUTPUT_S3_PREFIX: output_path
DATA_OUTPUT_FORMAT: parquet
```
