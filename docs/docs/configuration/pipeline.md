# Pipeline & Workspace

Pipelines are configured under `pipelines:` in `skippr.yml`.

```yaml
skippr:
  workspace: dev
  default_warehouse: primary

pipelines:
  events:
    data_source: data_sources.events
    data_sink: data_sinks.landing
    schema_sink: schema_sinks.catalog
    deadletter_sink: deadletter_sinks.deadletters
    transform:
      batch_time_fields: created_at
      batch_time_unit: day
    model:
      warehouse: primary
```

The pipeline name (`events` above) is the key used by both binaries:

```bash
skippr sync --pipeline events
skipprd --config skippr.yml sync --pipeline events
```

`model.warehouse` selects a key from top-level `warehouses:`. If omitted, `skippr.default_warehouse` is used.

## Environment overrides

## PIPELINE_NAME

The name of the pipeline. Used as part of the composite key for metadata, schemas, offsets, and WAL storage.

| | |
|---|---|
| **Environment variable** | `PIPELINE_NAME` |
| **Default** | `default` |
| **Example** | `bikehire`, `user_events`, `clickstream` |

Combined with `WORKSPACE_NAME` and `TENANT` to form the full pipeline path: `{tenant}/{workspace}/{pipeline}`.

## WORKSPACE_NAME

A logical grouping for pipelines, typically representing an environment or domain.

| | |
|---|---|
| **Environment variable** | `WORKSPACE_NAME` |
| **Default** | `default` |
| **Example** | `dev`, `prod`, `marketing` |

## TENANT

Tenant identifier for multi-tenant deployments.

| | |
|---|---|
| **Environment variable** | `TENANT` |
| **Default** | `default` |
