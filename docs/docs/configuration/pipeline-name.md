##### PIPELINE_NAME

#### Description
This configuration parameter specifies the name of the ingestion pipeline.

#### Default Value
If not set, the default value of `PIPELINE_NAME` is `default`.

#### Example Values
- **"DataPipeline1"** - Sets the pipeline name to "DataPipeline1". This will be appended to the `WORKSPACE_NAME` to form a composite namespace for the pipeline's metadata.
- **"IngestPipeline"** - Sets the pipeline name to "IngestPipeline". This, combined with `WORKSPACE_NAME`, creates a composite namespace for the metadata of the ingestion pipeline.

#### Detailed Description
The `PIPELINE_NAME` configuration value is a key component of the Skippr data ingestion tool. It defines the name of the pipeline that is responsible for ingesting data into the system. It works in conjunction with the `WORKSPACE_NAME` to create a unique namespace for each pipeline's metadata, which includes schemas, buffers, and the offsets database. The composite namespace is formed by appending the `PIPELINE_NAME` to the `WORKSPACE_NAME`.

For instance, if your `WORKSPACE_NAME` is "WorkspaceA" and your `PIPELINE_NAME` is "DataPipeline1", the composite namespace for your pipeline's metadata would be "WorkspaceA-DataPipeline1".

#### Considerations
When configuring `PIPELINE_NAME`, consider the following:

1. **Uniqueness**: Ensure the pipeline name is unique within the scope of the `WORKSPACE_NAME`. This is crucial to avoid conflicts with other pipelines' metadata.

2. **Descriptive Naming**: Choose a name that is descriptive and relevant to the nature of the data being ingested or the specific use case of the pipeline.

3. **Consistency**: Maintain a consistent naming pattern across all your pipelines. This will enhance manageability and readability, particularly in environments with multiple active pipelines.

4. **Alphanumeric Characters**: The pipeline name should only contain alphanumeric characters and underscores. Special characters may not be recognized and could lead to unexpected results.

Remember, the `PIPELINE_NAME` is an integral part of managing and controlling the data ingestion process in Skippr. Proper naming and organization can dramatically increase the maintainability and robustness of your ingestion pipelines.
