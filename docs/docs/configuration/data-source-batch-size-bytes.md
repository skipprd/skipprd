
##### DATA_SOURCE_BATCH_SIZE_BYTES

#### Config Name
`DATA_SOURCE_BATCH_SIZE_BYTES`

#### Description
Specifies the batch size, in bytes, that Skippr will attempt to ingest data from the data source in a single operation.

#### Default Value
If not explicitly specified, Skippr will use a default batch size value defined by the input plugin.

#### Example Values
1. `DATA_SOURCE_BATCH_SIZE_BYTES=1048576` : This configuration will set the batch size to 1 MB. Using smaller batch sizes can potentially lead to more frequent, smaller data ingest operations.
2. `DATA_SOURCE_BATCH_SIZE_BYTES=536870912` : This configuration sets the batch size to 512 MB. Larger batch sizes can result in less frequent, but larger data ingest operations.

#### Detailed Description
`DATA_SOURCE_BATCH_SIZE_BYTES` is a configuration option that determines the size of data batches that Skippr attempts to ingest from the source data system at a time. This option can be used to control the performance trade-offs between latency and throughput of data ingestion. Setting a smaller batch size might result in quicker ingestion times per batch, but it might require more operations to ingest all data. Conversely, a larger batch size might take longer to process but will require fewer operations overall.

#### Considerations
- *Latency vs Throughput*: While smaller batch sizes may lead to faster ingestion of individual batches, this could result in increased overall latency if the number of total batches to be ingested is high. On the other hand, larger batch sizes may lead to improved throughput, but with longer latency for individual batches. The appropriate value for your use case will depend on the specifics of your data and your system's requirements.

- *System Resources*: Larger batch sizes could require more memory resources. Ensure that your system has sufficient resources to handle the batch size you specify.

- *Concurrency and Queuing*: Depending on the source data system and its ability to handle simultaneous read requests, increasing the batch size might lead to more time spent in queuing. This could adversely affect data ingestion performance. Be aware of the capabilities of your source system when setting this value.

Remember that the optimal batch size will likely vary depending on many factors, including the characteristics of the data source system, the network bandwidth, the resources of the system where Skippr is running, and the specific requirements of your use case.
