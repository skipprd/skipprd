# File Output

Writes Parquet or JSON Lines files to the local filesystem. Useful for local development and testing.

## Configuration

```yaml
data_sinks:
  lake:
    File:
      format: jsonl
      output_dir: /tmp/cube-events
```

| Field | Default | Description |
|---|---|---|
| `format` | `parquet` | `parquet` or `jsonl` (`json` is accepted as an alias for `jsonl`) |
| `output_dir` | `{data_dir}/output` | Lake root. Hive partitions are written under this directory. |

## Output layout

Objects are written as:

```
{output_dir}/{namespace}/{p_<field>=...}/{p_year=...}/{stem}.{parquet|jsonl}
```

`batch_partition_fields` become `p_<field>=` directories. Time partitioning uses `TRANSFORM_BATCH_TIME_UNIT` and optional `time_partition_prefix` (use `p_` for `p_year` / `p_month` / `p_day`).

## Authentication

No connector-specific authentication is required.

## Troubleshooting

| Symptom | Fix |
|---|---|
| file cannot be created | Verify the parent directory exists and that the runner has write permission there. |
| output is not where you expect | Set `output_dir` to an absolute path. The default root is `{data_dir}/output`. |
| `format` rejected | File accepts only `parquet` and `jsonl`. |
