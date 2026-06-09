# Revolut Business Input

Revolut Business accounts and transactions for treasury and reconciliation pipelines.

## Configuration

```yaml
data_sources:
  revolut:
    RevolutBusiness:
      client_id: ${REVOLUT_CLIENT_ID}
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 90
      refresh_token: ${REVOLUT_REFRESH_TOKEN}
```

| Field | Default | Description |
| --- | --- | --- |
| `client_id` | *(required)* | Revolut API client ID |
| `start_date` | *(required)* | First transaction date (`YYYY-MM-DD`) |
| `lookback_days` | `90` | Transaction lookback window |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `streams` | profile set | Explicit namespace list |
| `min_query_interval_ms` | `300` | Minimum delay between API calls |
| `api_base` | Production API URL | API base URL override |
| `private_key_pem` | | Client assertion private key (JWT auth) |
| `refresh_token` | | OAuth refresh token |
| `access_token` | | Short-lived bearer token |
| `issuer_domain` | | JWT issuer domain (`REVOLUT_ISSUER_DOMAIN` env alternative) |
| `privacy` | | Field redaction settings |

## Pipeline wiring

```yaml
pipelines:
  treasury:
    data_source: data_sources.revolut
    data_sink: data_sinks.landing
```

Provide `access_token` or `refresh_token` (with `client_id` and signing key as required by your Revolut app configuration).
