##### Configuration - DATA_SOURCE_PLUGIN_NAME

#### Description
Determines the data source plugin Skippr uses for data ingestion.

#### Default Value
No default value. This configuration must be specified explicitly.

#### Example Values
- `"stdin"`: Skippr ingests data from the standard input stream.
- `"s3"`: Skippr ingests data from an S3 bucket.
- `"s3_inventory"`: Skippr ingests data from an S3 inventory.
- Any unsupported value will return a console message: "Plugin {plugin_name} not supported".

#### Detailed Description
The `DATA_SOURCE_PLUGIN_NAME` configuration instructs Skippr on the source of data for ingestion. Depending on the value of `DATA_SOURCE_PLUGIN_NAME`, Skippr employs the corresponding data source plugin for data ingestion.

If the configuration is set to `"stdin"`, the data is read from the standard input stream. This allows you to pipe data from other processes directly into Skippr for processing.

If the value is set to `"s3"`, Skippr connects to an Amazon S3 bucket as the data source. It will read and ingest files stored in the specified S3 bucket.

For `"s3_inventory"`, Skippr uses an S3 inventory as the data source. An S3 inventory provides a scheduled alternative to listing all objects in an S3 bucket. Skippr can parse this inventory to understand what data is available for ingestion.

In case of an unsupported value, a console message is printed indicating the plugin is not supported.

#### Considerations
- The correct plugin must be installed and configured for Skippr to ingest data successfully. For example, the `"s3"` and `"s3_inventory"` plugins require appropriate AWS credentials and bucket names.
- If ingesting data from `"stdin"`, ensure that the data is properly formatted and compatible with the data parsers used in your Skippr pipeline.
- Using `"s3_inventory"` as a source can be beneficial for large buckets where listing all objects is time and resource-intensive.
- Unsupported values will not halt the program execution but will result in no data being ingested, which can affect subsequent steps in the data processing pipeline.
- Be aware of any possible rate limits or access constraints on your data source to prevent potential issues during data ingestion.
