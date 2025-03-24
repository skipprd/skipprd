# Skippr Configuration Guide

## DATA_SOURCE_BATCH_SIZE_SECONDS

### Description
The maximum time in seconds a data source plugin batches input data before ingesting and fetching the next batch.

### Default Value
The default value of DATA_SOURCE_BATCH_SIZE_SECONDS is defined by the specific input plugin used. If the plugin does not provide a default value, an error will occur.

### Example Values
- `DATA_SOURCE_BATCH_SIZE_SECONDS=10` would allow a data source plugin to batch input data for 10 seconds before ingesting it and fetching the next batch.
- `DATA_SOURCE_BATCH_SIZE_SECONDS=60`, the input plugin will batch data for 1 minute before forcing to ingestion.

### Detailed Description
The `DATA_SOURCE_BATCH_SIZE_SECONDS` configuration parameter determines the maximum time duration in seconds that the Skippr data source plugin batches input data before ingestion. This setting functions together with `DATA_SOURCE_BATCH_SIZE_BYTES`, defining the time and data size limits for the batch processing.

Batching data can have significant effects on both performance and resource consumption of the system. By accumulating data over a specified time period or until a specified size is reached, Skippr can ingest data in larger, less frequent chunks, leading to potentially improved throughput and less overhead.

### Considerations
While configuring `DATA_SOURCE_BATCH_SIZE_SECONDS`, the following aspects should be considered:

1. **Latency vs Throughput**: Higher values can lead to increased throughput due to fewer, larger ingest operations, but at the cost of increased latency, as data waits in queue for the batch to be completed before being ingested.

2. **Performance Impact**: Depending on the capacity and performance of the data source and destination systems, large batch sizes may cause performance issues. Tuning the batch size in accordance to these factors can ensure optimal performance.

3. **Data Volume and Velocity**: If the data source produces high volumes of data at high velocity, a lower batch time can ensure timely ingestion and processing.

Remember, the ideal configuration of `DATA_SOURCE_BATCH_SIZE_SECONDS` may vary depending on the specifics of your use-case, the nature of your data, and the capacity of your systems. It is advisable to experiment with different settings to find the optimal configuration for your needs.
