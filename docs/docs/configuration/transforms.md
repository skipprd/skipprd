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

Timestamp field(s) used for time-based partitioning. Skipprd uses the first field found in each record. Supports Unix timestamps in both seconds and milliseconds.

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

## TRANSFORM_BATCH_ORDER_FIELDS

Columns used to sort rows within each Parquet file before writing. Sorting improves query performance in Athena and other engines that use Parquet row-group min/max statistics for predicate pruning.

| | |
|---|---|
| **Environment variable** | `TRANSFORM_BATCH_ORDER_FIELDS` |
| **Config key** | `transform.batch_order_fields` |
| **Default** | *(unset — no ordering)* |
| **Example** | `customer_id,event_time` |

Comma-separated list of output column names. For each namespace Skipprd writes, only the fields that exist in that namespace's output schema are used; missing fields are silently ignored. If no configured fields match a given namespace, records are written unsorted.

At the end of a run, Skipprd logs a warning listing any configured order fields that never matched any namespace observed during the run.

### How ordering helps

Without ordering, a filter like `SELECT * FROM foo WHERE bar = 4` may scan the same amount of data as `SELECT * FROM foo` because matching values of `bar` are scattered across every row group in the file.

When rows are sorted by `bar` before writing, values of `bar` cluster together. Each Parquet row group records the min and max value of every column, so the query engine can skip entire row groups that cannot contain `bar = 4`.

### Multi-column ordering

Fields are applied in the order listed. The first field provides the strongest clustering and benefits the most from pruning. Adding a second field helps queries that filter on both columns together but dilutes the clustering of the first column.

In practice, one or two fields is usually optimal. Long sort lists can reduce the pruning benefit for any single field.

### Automatic row-group sizing

When ordering is active, Skipprd automatically tunes the Parquet row-group size based on:

- **Average row width** in the current batch.
- **Run lengths** of the leading sort column — high-cardinality columns produce shorter runs and smaller row groups; low-cardinality columns produce longer runs and larger row groups.

Row groups are kept between approximately 16 MiB and 64 MiB uncompressed (25,000–500,000 rows). This balances metadata overhead against predicate pruning granularity without requiring manual configuration.
