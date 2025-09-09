# Configuration: SKIPPR_S3_BUCKET

## Description

This configuration specifies the S3 bucket where Skippr will store metadata, logs, and metrics. All persistence operations are now handled through S3 instead of external APIs.

## Default Value

Default value: `skippr-data`

## Example Values

Let's consider an example where you want to use a bucket named "my-skippr-data".

- SKIPPR_S3_BUCKET=my-skippr-data

In this example, Skippr will store all data at paths like:
- `s3://my-skippr-data/skippr/{workspace}/{pipeline}/metadata/metadata.json`
- `s3://my-skippr-data/skippr/{workspace}/{pipeline}/logs/{timestamp}_{run_id}.json`
- `s3://my-skippr-data/skippr/{workspace}/{pipeline}/metrics/{timestamp}_{run_id}.json`

## Detailed Description

The SKIPPR_S3_BUCKET configuration defines the S3 bucket used for all Skippr persistence operations. This includes:

- **Metadata**: Pipeline schema and configuration metadata
- **Logs**: Application logs with structured JSON format
- **Metrics**: Performance and operational metrics
- **Config**: Pipeline configuration snapshots

All data is organized by workspace and pipeline name within the bucket for easy management and access control.

## Considerations

When setting up Skippr with S3 persistence, consider the following:

- Ensure your AWS credentials have read/write access to the specified S3 bucket.

- The bucket should exist before running Skippr, or your AWS credentials should have permissions to create buckets.

- Consider implementing appropriate S3 bucket policies and lifecycle rules for data retention and cost management.

- All team members should have access to the same S3 bucket for shared pipeline metadata.

- Use consistent workspace and pipeline names across your team to ensure proper data organization.

