# File Input Plugin

Ingests data from the local filesystem.

##### Data Formats

- json
- avro
- csv

See: [Skippr serialisation formats](/formats)

### Config

```bash
DATA_SOURCE_PLUGIN_NAME: file
DATA_SOURCE_PATH: /data/input-dir
DATA_SOURCE_FORMAT: parquet
```

**NOTE: Volume mounts**

The `input-dir` must be located in our hosts mounted volume (e.g. `~/Documents/demo/skippr-data`).

The `DATA_SOURCE_PATH` should be prefixed with the docker mounted volume `/data`
