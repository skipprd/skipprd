# S3 Bucket Input Plugin

Reads data from an S3 bucket.

##### Data Formats

- json
- avro
- csv

See: [Skippr serialisation formats](/formats)

### Config

```bash
DATA_SOURCE_PLUGIN_NAME: "s3"
DATA_SOURCE_S3_BUCKET: bucket-name
DATA_SOURCE_AWS_REGION: eu-west-2
DATA_SOURCE_AWS_ACCESS_ID: [SECRET]
DATA_SOURCE_AWS_SECRET_KEY: [SECRET]
DATA_SOURCE_S3_PREFIX: output_path
DATA_SOURCE_FORMAT: json
```
