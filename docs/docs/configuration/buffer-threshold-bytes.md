# Configuration

#### Config Name:
BUFFER_THRESHOLD_BYTES

##### Description
The `BUFFER_THRESHOLD_BYTES` configuration parameter defines the maximum amount of bytes that a single buffer can hold.

##### Default Value
The default value for the `BUFFER_THRESHOLD_BYTES` configuration parameter is `10485760` bytes, equivalent to 10 Megabytes (MB).

##### Example Values
- `BUFFER_THRESHOLD_BYTES=20971520` sets the maximum buffer size to 20MB. Increasing the buffer size can help with the ingestion of larger datasets but may consume more memory.
- `BUFFER_THRESHOLD_BYTES=5242880` sets the maximum buffer size to 5MB. Lowering the buffer size reduces memory usage but may slow down the data ingestion process as it requires more frequent buffer flushing.

##### Detailed Description
The `BUFFER_THRESHOLD_BYTES` configuration parameter is a critical setting that balances the performance and memory usage in the data ingestion process. It establishes the limit for the amount of data that Skippr will buffer before initiating a write to the desired output (i.e., datalakes, streams, and file systems).

As data is ingested into Skippr, it is initially stored in a buffer. Once the data in the buffer reaches the size defined by `BUFFER_THRESHOLD_BYTES`, Skippr will start the process of writing the buffered data to the output destination. This buffer and flush mechanism allows for efficient ingestion and data transfer, especially for large datasets or high-speed data streams.

##### Considerations
1. Buffer Size and Memory Usage: A larger buffer size (set by a higher `BUFFER_THRESHOLD_BYTES` value) can ingest data more efficiently, reducing the need for frequent disk I/O operations. However, it also increases the application's memory footprint. Therefore, it's important to find a balance between performance and memory consumption based on your system's available resources.

2. Buffer Size and Data Latency: A smaller buffer size will result in more frequent writes to the output destination, which can increase data latency. If low latency is critical for your use case, you may want to consider increasing `BUFFER_THRESHOLD_BYTES`.

3. System Load: Adjusting `BUFFER_THRESHOLD_BYTES` can have a significant impact on system load. More frequent writes to the output (due to a smaller buffer size) may increase CPU usage and I/O operations. Conversely, a larger buffer size can lead to less frequent but larger data transfers, which may also cause spikes in system load.

Remember, there is no one-size-fits-all value for `BUFFER_THRESHOLD_BYTES`. You should adjust this parameter based on your specific use case, the nature of your data, and your system's resources.

