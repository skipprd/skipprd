---
title: How sources land
description: Choose how each source table is written — append, merge, replace a day, or replace the table — and pair it with a destination that supports that write.
---

# How sources land

A source does two jobs: it names the columns, and it says **how each table must be written**. Column types come from `skipprd discover`. How rows land is the write policy.

API and SaaS sources (Google Analytics, ads, Stripe, and similar) declare a policy per table because their APIs revise past days or republish a full snapshot. Database snapshot sources usually append. CDC sources merge or apply deletes.

Skipprd checks the policy when the pipeline starts. If your destination cannot do what the source requires, startup fails with a clear error instead of writing a table you cannot trust.

## Write policies

| Policy | What a run does | Use when |
|---|---|---|
| `append` | Adds rows. A second run adds another copy. | Immutable events, logs, or a destination that only appends |
| `merge_by_key` | Upserts on the table's key | Current state by business key (CDC, CRM snapshots) |
| `replace_partition` | Drops and rewrites the slice for that run (often one calendar day) | Reports that Google or an ads API will revise |
| `replace_table` | Replaces the whole table with the latest run | Small, bounded snapshots where you do not keep history |

**Mutable reports.** APIs such as GA4 change metrics for dates you already synced. `replace_partition` tells the destination to rewrite `date=2024-01-15` (or the equivalent) before writing the new rows.

**Lookback.** Sources such as GA4 re-fetch the last *N* days on every run (`lookback_days`). That only stays correct if the write policy replaces those days. A checkpoint is not a substitute for the policy.

## Which destinations support replace

| Destination | `replace_partition` | `merge_by_key` | `replace_table` | `append` |
|---|---|---|---|---|
| Athena | Yes | No | Yes | Yes |
| Athena Iceberg | Yes | Yes | Yes | Yes |
| SkipprLake | Yes | Yes | Yes | Yes |
| DuckDB | No | No | Yes | Yes |
| Other destinations | No | Varies | Varies | Yes |

If you point a `replace_partition` source at an append-only destination, Skipprd refuses to start. Either change the destination, or set `write_policy: append` on sources that allow it (Stripe does; GA4 does not, because revised days would duplicate).

## Discover vs landing

| Layer | Controls |
|---|---|
| Write policy, key, partition | **How** each batch is applied |
| Discovered schema | **Column names and types** |

Changing a column type is a schema change. Changing how a day is rewritten is a landing change. They are independent.

## Example: Google Analytics 4

GA4 daily tables use `replace_partition` on `date`. Each scheduled sync rewrites recent days Google may have revised. See [Google Analytics (GA4)](/connectors/inputs/google_analytics) and [GA4 bronze and modeling](/concepts/ga4-bronze-and-modeling).

## Next steps

- [Google Analytics (GA4)](/connectors/inputs/google_analytics)
- [Stripe](/connectors/inputs/stripe)
- [Athena](/connectors/outputs/athena)
- [Exactly-once delivery](/concepts/exactly-once)
