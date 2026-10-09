---
title: Stripe
description: Sync Stripe charges, invoices, subscriptions, payouts, and catalog snapshots into your warehouse as daily, queryable tables.
---

# Stripe

Use the Stripe source to land your Stripe account in your warehouse: payments, refunds, invoices and invoice lines, subscriptions with MRR, payouts and balance transactions, plus snapshots of customers, products, prices, coupons, and promotion codes.

Each run reads every object created since your start date and lands it as that day's snapshot, tagged with the run date. Re-running on the same day replaces that day's rows, so you get one clean snapshot per day and a history you can query over time.

## Before you begin

1. **Create a key in the Stripe Dashboard.** Go to **Developers → API keys**. Use a [restricted key](https://docs.stripe.com/keys#limit-access) with **Read** access to the resources you sync: Account, Charges, PaymentIntents, Refunds, Disputes, Invoices, Subscriptions, Customers, Products, Prices, Coupons, Promotion codes, Balance transactions, and Payouts. A secret key (`sk_live_…`) also works but grants write access you don't need.
2. **Find your account ID.** In the Dashboard, open **Settings → Business → Account details**. The ID starts with `acct_`. Use the ID of the account the key belongs to; Skipprd stamps it on every row as `stripe_account_id`.
3. **Choose a start date.** Skipprd reads objects created on or after `start_date` minus `lookback_days` on every run, so an earlier date means longer runs.
4. **Pick a destination that supports how Stripe lands.** By default every Stripe table uses replace-partition writes (each run rewrites that day's partition). [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), and [SkipprLake](/connectors/outputs/skipprlake) support this. For any other destination, set `write_policy: append` (see [Write policies](#write-policies)).

## Configure

Export the key so the config can reference it without storing it:

```bash
export STRIPE_SECRET_KEY="rk_live_..."
```

Add the source and wire it into a pipeline. These examples assume you already have a destination named `lake`.

::: code-group

```python [Python]
from skippr import Config, DataSourceStripe, EnvRef, Pipeline

cfg = Config.discover()
stripe = cfg.data_source(
    "stripe",
    DataSourceStripe(
        stripe_account_id="acct_1234567890",
        start_date="2024-01-01",
        access_token=EnvRef("STRIPE_SECRET_KEY"),
    ),
)
cfg.pipeline(
    "payments",
    Pipeline(data_source=stripe, data_sink=cfg.get_data_sink("lake")),
)
cfg.save()
```

```bash [CLI]
skipprd connect data-source stripe \
  --pipeline payments \
  --name stripe \
  --stripe-account-id acct_1234567890 \
  --start-date 2024-01-01 \
  --access-token '${STRIPE_SECRET_KEY}'
```

```yaml [YAML]
data_sources:
  stripe:
    Stripe:
      stripe_account_id: acct_1234567890
      start_date: "2024-01-01"
      access_token: ${STRIPE_SECRET_KEY}

pipelines:
  payments:
    data_source: data_sources.stripe
    data_sink: data_sinks.lake
```

:::

With the CLI, connect the destination to the same pipeline with `skipprd connect data-sink … --pipeline payments`.

Check the connection and run one pass:

```bash
skipprd discover --pipeline payments
skipprd sync --pipeline payments --once
```

A successful run lands a row in `stripe_sync_run_daily` with `status = ok` and the list of streams it synced.

### Use OAuth instead of a key

If you connect through a Stripe OAuth app, leave `access_token` unset and provide the refresh credentials. Skipprd exchanges the refresh token for an access token at the start of each run.

```yaml
data_sources:
  stripe:
    Stripe:
      stripe_account_id: acct_1234567890
      start_date: "2024-01-01"
      oauth_token_url: https://connect.stripe.com/oauth/token
      oauth_client_id: ca_1234567890
      oauth_client_secret: ${STRIPE_OAUTH_CLIENT_SECRET}
      oauth_refresh_token: ${STRIPE_OAUTH_REFRESH_TOKEN}
```

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `stripe_account_id` | string | Required | The `acct_…` ID of the account your key reads. Written to every row as `stripe_account_id`. |
| `start_date` | string (`YYYY-MM-DD`) | Required | Earliest creation date to read. |
| `lookback_days` | integer | `7` | Days subtracted from `start_date`. Each run reads objects created on or after `start_date` minus this many days. |
| `stream_profile` | string | `console_default` | Which streams to sync: `minimal`, `console_default`, or `full`. See [What gets synced](#what-gets-synced). |
| `streams` | list of strings | Not set | Sync exactly these streams instead of a profile: `account`, `catalog`, `customers`, `subscriptions`, `invoices`, `payments`, `disputes`, `cash`, `promotions`. Unknown names are ignored. |
| `min_query_interval_ms` | integer | `100` | Minimum milliseconds between Stripe API requests. Raise it if you hit rate limits. |
| `write_policy` | string | `replace_partition` | How every Stripe table lands: `replace_partition`, `append`, `merge_by_key`, or `replace_table`. Your destination must support the policy you choose. |
| `access_token` | secret | Not set | Stripe restricted or secret key, as `${ENV_VAR}`. When set, OAuth settings are ignored. |
| `oauth_token_url` | string | Not set | OAuth token endpoint. Required when `access_token` is not set. |
| `oauth_client_id` | string | Not set | OAuth client ID. |
| `oauth_client_secret` | secret | Not set | OAuth client secret, as `${ENV_VAR}`. |
| `oauth_refresh_token` | secret | Not set | OAuth refresh token, as `${ENV_VAR}`. |
| `privacy.mode` | string | `passthrough` | `passthrough`, `profile`, or `allowlist`. See [Personal data](#personal-data). |
| `privacy.profile` | string | `passthrough` | `passthrough` or `upfoundry_safe`. Setting `upfoundry_safe` switches on profile mode even when `mode` is `passthrough`. |
| `privacy.drop_properties` | list of strings | `[]` | Extra field names to remove (case-insensitive) when the `upfoundry_safe` profile is active. |
| `privacy.keep_properties` | list of strings | `[]` | In `allowlist` mode, the only top-level fields kept on each row. |
| `privacy.hash_properties` | list of strings | `[]` | Accepted but not applied in this version. |
| `privacy.on_violation` | string | `drop` | `drop` or `deadletter`. Accepted but not applied in this version. |

## What gets synced

Each stream lands in one or more tables. The profile you pick decides which streams run.

| Stream | Tables | `minimal` | `console_default` | `full` |
|---|---|:-:|:-:|:-:|
| `account` | `stripe_account_snapshot` | ✓ | ✓ | ✓ |
| `subscriptions` | `stripe_subscription_snapshot` | ✓ | ✓ | ✓ |
| `invoices` | `stripe_invoice_fact`, `stripe_invoice_line_fact` | ✓ | ✓ | ✓ |
| `payments` | `stripe_charge_fact`, `stripe_payment_intent_fact`, `stripe_refund_fact` | ✓ | ✓ | ✓ |
| `customers` | `stripe_customer_snapshot` | | ✓ | ✓ |
| `catalog` | `stripe_product_snapshot`, `stripe_price_snapshot` | | ✓ | ✓ |
| `cash` | `stripe_balance_transaction_fact`, `stripe_payout_fact` | | ✓ | ✓ |
| `disputes` | `stripe_dispute_fact` | | | ✓ |
| `promotions` | `stripe_coupon_snapshot`, `stripe_promotion_code_snapshot` | | | ✓ |

Every run also writes one row to `stripe_sync_run_daily` with `run_date`, `status`, `streams_synced`, and `elapsed_ms`.

**Rows and columns.** Skipprd lands a curated set of columns per object, not the full Stripe JSON. Every row carries `ingest_run_date` (the UTC date of the run) and `stripe_account_id`. IDs use Stripe's own values (`charge_id`, `invoice_id`, …). Amounts are integers in the currency's smallest unit, as Stripe returns them. Timestamps are ISO 8601 UTC strings. Subscriptions include `mrr_cents`, computed from their price items.

**How runs build on each other.** There is no cursor. Every run reads all objects created on or after `start_date` minus `lookback_days` and lands them under today's `ingest_run_date`. So each day's partition is a full snapshot of that window, including status changes to older objects. To get current state, filter to the latest `ingest_run_date`.

**Deletes.** An object you delete in Stripe stops appearing in new snapshots. Earlier days keep it.

**Discover.** `skipprd discover` reads the `minimal` streams, without the start-date filter, to learn column types. On a large account this can take a while.

### Write policies

`write_policy` applies to every Stripe table. Pick one your destination supports; Skipprd checks this when the pipeline starts.

| Policy | What a run does | Destinations |
|---|---|---|
| `replace_partition` (default) | Replaces the rows for the run's `ingest_run_date`. Re-running on the same day is safe. | Athena, Athena Iceberg, SkipprLake |
| `merge_by_key` | Upserts on the object ID plus `ingest_run_date`. | Athena Iceberg, SkipprLake |
| `replace_table` | Replaces the whole table with the latest run, so you keep no history. | Athena, Athena Iceberg, SkipprLake, DuckDB |
| `append` | Adds rows. A second run on the same day adds a second copy of that day's snapshot. | Any destination |

See [How sources land](/concepts/source-landing-semantics) for more on write policies.

### Personal data

The curated columns leave out customer emails, names, addresses, and card details. On top of that, the default `passthrough` mode removes any top-level field whose name is exactly a personal-data term such as `name`, `email`, `phone`, `address`, `country`, `description`, or `metadata`. That is why product and coupon names, the account country, and invoice-line descriptions don't land by default.

| Setting | Effect |
|---|---|
| `mode: passthrough` (default) | Removes top-level fields named exactly like personal data. |
| `profile: upfoundry_safe` | Also removes any field, at any depth, whose name *contains* a personal-data term, plus every field in `drop_properties`. |
| `mode: allowlist` | Keeps only the top-level fields in `keep_properties`. Always include the ID column and `ingest_run_date`: together they are each table's key. |

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `sink '…' does not support write policy ReplacePartition for namespace 'stripe_…'` | Your destination only appends. | Use Athena, Athena Iceberg, or SkipprLake, or set `write_policy: append`. |
| `Stripe requires oauth_refresh_token + client credentials …` | No key reached Skipprd, so it fell back to OAuth with no token URL. | Export the variable named in `access_token` in the shell that runs `skipprd`, and check the name matches. |
| `Stripe HTTP 401` | The key is wrong, revoked, or from the other mode (test vs live). | Create a new key in the Dashboard and update the variable. |
| `Stripe HTTP 403` | A restricted key lacks **Read** on a resource a selected stream needs. | Grant the permission, or remove that stream from `streams`. |
| `Stripe HTTP 429` | Requests are too fast for your account's rate limit. | Raise `min_query_interval_ms`, for example to `500`. |
| Only `stripe_sync_run_daily` lands | Every name in `streams` is misspelled; unknown names are ignored. | Use the stream names listed in [Options](#options). |
| Product names or account country are missing | The default privacy mode removes top-level `name` and `country` fields. | This is expected. Use the IDs to join to your own reference data. |
| Runs get slower over time | Every run re-reads the whole window from `start_date`. | Move `start_date` later once you have the history you need. |

## Next steps

- [How sources land](/concepts/source-landing-semantics)
- [SkipprLake destination](/connectors/outputs/skipprlake)
- [skipprd sync](/cli/sync)
