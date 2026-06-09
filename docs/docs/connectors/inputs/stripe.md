# Stripe Input

Stripe account objects and payment facts — charges, invoices, subscriptions, payouts, and related snapshots.

## Configuration

```yaml
data_sources:
  stripe:
    Stripe:
      stripe_account_id: acct_1234567890
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 7
      access_token: ${STRIPE_SECRET_KEY}
```

| Field | Default | Description |
| --- | --- | --- |
| `stripe_account_id` | *(required)* | Stripe account ID (`acct_…`) |
| `start_date` | *(required)* | First sync date (`YYYY-MM-DD`) |
| `lookback_days` | `7` | Days of history to re-pull on each run |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `streams` | profile set | Explicit namespace list |
| `min_query_interval_ms` | `100` | Minimum delay between API calls |
| `write_policy` | | Optional per-source write policy override |
| `access_token` | | Stripe secret or restricted key |
| `oauth_*` | | Connect OAuth refresh credentials |
| `privacy` | | Field redaction settings for sensitive payloads |

## Pipeline wiring

```yaml
pipelines:
  payments:
    data_source: data_sources.stripe
    data_sink: data_sinks.landing
```

Namespaces include fact tables (charges, invoices, payouts) and snapshot tables (customers, products, prices). Use `stream_profile: minimal` for smoke tests.
