# Pipeline & Workspace

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
