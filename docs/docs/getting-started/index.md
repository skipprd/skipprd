# Let's get started!

Skippr is a data pipeline tool that makes it easy to ingest, transform and output optimised data.

## Hello World Example

In this simple example we'll convert json source data to Parquet files for analysis.

This is a simple copy and paste example, to get started quickly in your local terminal. You'll be ready to deploy Skippr and integrate enterprise systems in no time!

- Install Skippr cli tool
- Ingest example Bike Hire IoT data from S3
- Finally, we'll do some analysis on the output data.


### Install Skippr

```bash
curl -sL "https://raw.githubusercontent.com/skipprd/skipprd/main/install.sh" | bash
```

### Ingest Example Data With Skippr

Copy and run the below command below in your terminal.

```ga-event-getting_started_run_docket_2
DATA_SOURCE_PLUGIN_NAME=s3 \
DATA_SOURCE_S3_BUCKET=skippr-public-sample-data \
DATA_SOURCE_S3_PREFIX=bike-hire \
AWS_DEFAULT_REGION=us-east-1 \
PIPELINE_NAME=bikehire \
skippr sync
```

### Analyse the output data


```ga-event-getting_started_analyse_count_bikehire
skippr query --query 'SELECT COUNT(*) FROM bikehire;'
```

### Help

Join us on [Slack](https://join.slack.com/t/skipprgroup/shared_invite/zt-np4dqtsw-ASqYMtQlFkvDTWXhz4hxYw).
