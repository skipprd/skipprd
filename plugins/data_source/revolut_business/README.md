# Revolut Business (`RevolutBusiness`)

Read-only Revolut Business connector for treasury analytics: account balances, transaction history, and transfer legs (`READ` scope only).

## Config (`skippr.yml`)

```yaml
RevolutBusiness:
  client_id: "${REVOLUT_CLIENT_ID}"
  start_date: "2024-01-01"
  lookback_days: 90
  stream_profile: full
  streams:
    - accounts
    - transactions
    - transaction_legs
    - health
  privacy:
    mode: profile
    profile: upfoundry_safe
    on_violation: drop
  min_query_interval_ms: 300
  api_base: "${REVOLUT_API_BASE}"
  private_key_pem: "${REVOLUT_PRIVATE_KEY_PEM}"
  refresh_token: "${REVOLUT_REFRESH_TOKEN}"
  access_token: "${REVOLUT_ACCESS_TOKEN}"
  issuer_domain: "${REVOLUT_ISSUER_DOMAIN}"
```

Prod API base: `https://b2b.revolut.com/api/1.0`  
Sandbox: `https://sandbox-b2b.revolut.com/api/1.0`

## Auth

Revolut Business uses **certificate + client-assertion JWT** (not `client_secret`). The plugin accepts:

- `access_token` — use directly when console-web has already exchanged tokens
- `refresh_token` + `private_key_pem` + `issuer_domain` — refresh via `POST {api_base}/auth/token` with `client_assertion`

`issuer_domain` must match the redirect URI domain registered in the Revolut developer portal (e.g. `api.upfoundry.co`). Falls back to `REVOLUT_ISSUER_DOMAIN` env.

`private_key_pem` is optional in fixture mode (`SKIPPR_REVOLUT_FIXTURE_DIR`).

## Privacy

`upfoundry_safe` strips account `name`, transaction `reference` / `description`, `beneficiary`, `merchant`, and `card` data before ingest. Raw envelopes store a SHA-256 hash of the redacted canonical JSON only. The `/accounts/{id}/bank-details` endpoint is not called.

## Namespaces

| Class | Namespace | Write policy |
|-------|-----------|--------------|
| Snapshot | `revolut_account_snapshot` | ReplacePartition on `run_date` |
| Fact | `revolut_transaction_fact`, `revolut_transaction_leg_fact` | MergeByKey with `refresh_window = lookback_days` |
| Raw | `revolut_*_source_raw` | MergeByKey on `source_record_id` + `ingest_run_date` |
| Health | `revolut_sync_run_daily` | ReplacePartition on `run_date` |

Transaction pagination: `from` / `to` ISO8601, `count` max 1000, cursor via last item `created_at` as next `to`.

## Tests

```bash
cargo test -p skippr-plugin-data-source-revolut-business
```

Uses `SKIPPR_REVOLUT_FIXTURE_DIR=tests/fixtures` when set.
