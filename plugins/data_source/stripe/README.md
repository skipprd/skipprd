# Stripe (`Stripe`)

Read-only Stripe Connect connector for billing and revenue analytics.

## Config (`skippr.yml`)

```yaml
Stripe:
  stripe_account_id: "acct_123"
  start_date: "2024-01-01"
  lookback_days: 30
  stream_profile: full
  streams:
    - account
    - catalog
    - customers
    - subscriptions
    - invoices
    - payments
    - disputes
    - cash
    - promotions
  privacy:
    mode: profile
    profile: upfoundry_safe
    on_violation: drop
  oauth_token_url: "https://connect.stripe.com/oauth/token"
  oauth_client_id: "${STRIPE_CONNECT_CLIENT_ID}"
  oauth_client_secret: "${STRIPE_CONNECT_CLIENT_SECRET}"
  oauth_refresh_token: "${STRIPE_OAUTH_REFRESH_TOKEN}"
```

## Privacy

Customer PII (email, name, address, card details) is stripped under `upfoundry_safe`.

## Tests

```bash
cargo test -p skippr-plugin-data-source-stripe
```

Uses `SKIPPR_STRIPE_FIXTURE_DIR=tests/fixtures` when set.
