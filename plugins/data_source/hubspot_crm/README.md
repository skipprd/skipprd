# HubSpot CRM (`HubspotCrm`)

Read-only HubSpot connector: CRM, Marketing, and Service **event facts** plus dimension/snapshot tables.

## Config (`skippr.yml`)

```yaml
HubspotCrm:
  hub_id: "12345678"
  start_date: "2024-01-01"
  lookback_days: 30
  stream_profile: console_default
  streams:
    - crm
    - marketing
    - service
  privacy:
    mode: profile
    profile: upfoundry_safe
  oauth_token_url: "https://api.hubapi.com/oauth/v1/token"
  oauth_client_id: "${HUBSPOT_OAUTH_CLIENT_ID}"
  oauth_client_secret: "${HUBSPOT_OAUTH_CLIENT_SECRET}"
  oauth_refresh_token: "${HUBSPOT_OAUTH_REFRESH_TOKEN}"
```

## Privacy

| `privacy.mode` | Behavior |
|----------------|----------|
| `passthrough` | Default; strips known PII keys only |
| `profile` | Named profile (`upfoundry_safe`, `crm_revenue_only`) |
| `allowlist` | Only `keep_properties` emitted |

This plugin has **no dependencies on other data-source plugins**.

## Tests

```bash
cargo test -p skippr-plugin-data-source-hubspot-crm
```

Uses `SKIPPR_HUBSPOT_FIXTURE_DIR=tests/fixtures` when set.
