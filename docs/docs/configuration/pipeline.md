# Pipeline & Workspace

Pipelines are configured under `pipelines:` in `skippr.yml`.

```yaml
skippr:
  workspace: dev

pipelines:
  events:
    data_source: data_sources.events
    data_sink: data_sinks.landing
    schema_sink: schema_sinks.catalog
    deadletter_sink: deadletter_sinks.deadletters
    transform:
      batch_time_fields: created_at
      batch_time_unit: day
```

The pipeline name (`events` above) is the YAML key. `--pipeline` and Python `Session(pipeline=...)` use that key. Python `Session.pipeline` is set only in the constructor.

```bash
skipprd discover --pipeline events
skipprd schema --pipeline events
skipprd sync --pipeline events
```

`data_sources.events` and `data_sinks.landing` are logical names. They are not reserved; use any keys that match your project. `data_sink` is optional: without it the WAL is the dataset.

## Environment overrides

## WORKSPACE_NAME

Workspace / domain name. Combined with tenant and pipeline for storage keys.

| | |
|---|---|
| **Environment variable** | `WORKSPACE_NAME` |
| **Default** | `default` |

## TENANT

Tenant identifier. Comes from authenticated credentials for hosted runs; local runs may set it via environment.

| | |
|---|---|
| **Environment variable** | `TENANT` |
| **Default** | `default` |
