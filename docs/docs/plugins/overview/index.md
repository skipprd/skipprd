# Plugins

### Input Plugins

Input plugins simply connect to a data source, read data (normally sequentially in chronological order) and emit the data to Skipprd for ingest.

No serialization or processing of the data is performed by the plugin (beyond any underlying source integration and wire transfer, etc).


### Output Plugins

Output plugins stream read ingested data from Skipprd buffers and syncs to the destination.

