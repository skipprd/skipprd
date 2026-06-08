# Xero Accounting (`XeroAccounting`)

Read-only Xero Accounting connector for ledger analytics: invoices, payments, bank transactions, and chart of accounts.

## Config (`skippr.yml`)

```yaml
XeroAccounting:
  tenant_id: "{{XERO_TENANT_ID}}"
  start_date: "2024-01-01"
  lookback_days: 90
  page_size: 100
  stream_profile: full
  streams:
    - organisation
    - accounts
    - contacts
    - invoices
    - payments
    - bank
    - health
  privacy:
    mode: profile
    profile: upfoundry_safe
    on_violation: drop
  min_query_interval_ms: 350
  oauth_token_url: "https://identity.xero.com/connect/token"
  oauth_client_id: "${XERO_OAUTH_CLIENT_ID}"
  oauth_client_secret: "${XERO_OAUTH_CLIENT_SECRET}"
  oauth_refresh_token: "${XERO_OAUTH_REFRESH_TOKEN}"
```

## Auth

OAuth 2.0 refresh token against `https://identity.xero.com/connect/token`. Every Accounting API request sends `Authorization: Bearer` and `xero-tenant-id`.

## Privacy

Strict no-PII under `upfoundry_safe`: contact names, emails, phones, addresses, invoice references, line descriptions, and bank payee names are stripped before ingest. Contacts snapshot stores ID and status/type flags only. Raw source envelopes store a SHA-256 hash of the redacted canonical JSON.

## Namespaces

| Class | Namespace | Write policy |
|-------|-----------|--------------|
| Snapshot | `xero_organisation_snapshot`, `xero_account_snapshot`, `xero_contact_snapshot` | ReplacePartition on `run_date` |
| Fact | `xero_invoice_fact`, `xero_invoice_line_fact`, `xero_payment_fact`, `xero_bank_transaction_fact` | MergeByKey with `refresh_window = lookback_days` |
| Raw | `xero_*_source_raw` | MergeByKey on `source_record_id` + `ingest_run_date` |
| Health | `xero_sync_run_daily` | ReplacePartition on `run_date` |

## Tests

```bash
cargo test -p skippr-plugin-data-source-xero-accounting
```

Uses `SKIPPR_XERO_FIXTURE_DIR=tests/fixtures` when set.
