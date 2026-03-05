# Transforms

Transform configuration controls how records are namespaced, partitioned, and structured before output.

## TRANSFORM_NAMESPACE_FIELDS

Fields that define the event type or schema namespace. Each unique combination of field values produces a separate schema and Glue table.

| | |
|---|---|
| **Environment variable** | `TRANSFORM_NAMESPACE_FIELDS` |
| **Default** | *(unset — all records share one schema)* |
| **Example** | `event_type` or `category,sub_category` |

Comma-separated for composite namespaces.

## TRANSFORM_BATCH_PARTITION_FIELDS

Fields used for Hive-style partitioning in the output. Records are grouped by the values of these fields.

| | |
|---|---|
| **Environment variable** | `TRANSFORM_BATCH_PARTITION_FIELDS` |
| **Default** | *(unset)* |
| **Example** | `country,state` or `product.category` |

Supports nested field paths using dot notation.

## TRANSFORM_BATCH_TIME_FIELDS

Timestamp field(s) used for time-based partitioning. Skippr uses the first field found in each record. Supports Unix timestamps in both seconds and milliseconds.

| | |
|---|---|
| **Environment variable** | `TRANSFORM_BATCH_TIME_FIELDS` |
| **Default** | *(unset)* |
| **Example** | `event_time` or `metadata.created_at,timestamp` |

Must be set if `TRANSFORM_BATCH_TIME_UNIT` is set.

## TRANSFORM_BATCH_TIME_UNIT

The granularity for time-based partitioning.

| | |
|---|---|
| **Environment variable** | `TRANSFORM_BATCH_TIME_UNIT` |
| **Default** | *(unset)* |
| **Values** | `year`, `month`, `day`, `hour`, `minute` |

Requires `TRANSFORM_BATCH_TIME_FIELDS` to be set.

## TRANSFORM_FLATTEN_EVENTS

Whether to flatten nested structures into dot-separated column names.

| | |
|---|---|
| **Environment variable** | `TRANSFORM_FLATTEN_EVENTS` |
| **Default** | `no` |
| **Values** | `yes` / `no` (also accepts `true`/`false`, `1`/`0`) |

When enabled, a nested field like `contact.name` becomes a top-level column named `contact.name` instead of a nested struct.
