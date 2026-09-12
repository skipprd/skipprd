# SumUp (`SumUp`)

Read-only SumUp connector for payment, payout, and checkout analytics.

## Config (`skippr.yml`)

```yaml
SumUp:
  merchant_code: "MH4H92C7"
  start_date: "2024-01-01"
  lookback_days: 30
  stream_profile: console_default
  streams:
    - merchant
    - transactions
    - payouts
    - checkouts
    - health
  privacy:
    mode: profile
    profile: passthrough
    on_violation: drop
  oauth_token_url: "https://api.sumup.com/token"
  oauth_client_id: "${SUMUP_OAUTH_CLIENT_ID}"
  oauth_client_secret: "${SUMUP_OAUTH_CLIENT_SECRET}"
  oauth_refresh_token: "${SUMUP_OAUTH_REFRESH_TOKEN}"
```

## Privacy

Customer and merchant PII (email, name, phone, address, cardholder, receipt, metadata, business_name) is stripped when a strict privacy profile is enabled. Raw source envelopes store only a SHA-256 hash of the redacted canonical JSON.

## Tests

```bash
cargo test -p skippr-plugin-data-source-sumup
```

Uses `SKIPPR_SUMUP_FIXTURE_DIR=tests/fixtures` when set.
