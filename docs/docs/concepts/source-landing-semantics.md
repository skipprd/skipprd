# Source landing semantics

Some sources—especially API and SaaS connectors—declare **how each namespace’s data must land** in the warehouse. Skipprd calls this a **namespace contract**. Contracts are separate from column types discovered by `skipprd discover`.

## Namespace contracts

For every table (namespace) a source can emit, the source plugin publishes a contract that includes:

| Field | Meaning |
|---|---|
| Primary key | Logical row identity (business dimensions plus identifiers such as `property_id` and `date`) |
| Partition key | Columns that define a physical slice for partition-scoped writes (often `date` for daily reports) |
| Write policy | How the configured **data sink** must apply each batch |
| Refresh window | Optional number of days to re-fetch before the checkpoint (for APIs that revise past days) |
| Semantics | Descriptor such as `mutable_report` (informational) |

The host validates contracts when a source starts and checks that your pipeline’s **data sink** supports every declared write policy.

## Write policies

| Policy | Use when |
|---|---|
| `append` | Rows are immutable or append-only |
| `merge_by_key` | Current state by business key (sink must support merge) |
| `replace_partition` | A partition can be fully rewritten when numbers change |
| `replace_table` | Small, bounded full snapshots |

**Mutable reports:** APIs like GA4 can change metrics for dates you already synced. `replace_partition` tells the sink to drop and rewrite the partition for the batch’s partition key values (for example `date=2024-01-15`) before writing new Parquet.

**Lookback / refresh window:** The source re-pulls the last *N* days on each run (`lookback_days` in GA4). Checkpoints record progress; they are not a substitute for the correct write policy.

## Destination pairing

| Data sink | `replace_partition` |
|---|---|
| Athena (S3 + Glue) | Yes |
| Iceberg | Yes |
| Append-only sinks | No — pipeline validation fails |

## Discover vs contracts

| Layer | Controls |
|---|---|
| Namespace contract | **How** batches land |
| Arrow / discovered schema | **Column names and types** |

## Example: Google Analytics 4

See [Google Analytics (GA4) input](../connectors/inputs/google_analytics.md) and [Athena output](../connectors/outputs/athena.md).

For plugin authors, see [API / SaaS source plugins](../maintainers/api-saas-source-plugins.md).
