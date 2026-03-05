# File Output

Writes Parquet files to the local filesystem. Useful for local development and testing.

## Configuration

```bash
DATA_OUTPUT_PATH=/path/to/output-directory
```

| Variable | Default | Description |
|---|---|---|
| `DATA_OUTPUT_PATH` | *(required)* | Output directory for Parquet files |

## Output format

Files are written as Parquet with Snappy compression, matching the format used by the Athena output.
