# File Output Plugin

Writes ingested data to the local filesystem.

##### Data Formats

- json
- avro
- parquet

See: [Skippr serialisation formats](/formats)

### Config

```bash
DATA_OUTPUT_PLUGIN_NAME: file
DATA_OUTPUT_PATH: /data/output-dir
DATA_OUTPUT_FORMAT: parquet
```

**NOTE: Volume mounts**

The `output-dir` must be located in our hosts mounted volume (e.g. `~/Documents/demo/skippr-data`).

The `DATA_OUTPUT_PATH` should be prefixed with the docker mounted volume `/data`
