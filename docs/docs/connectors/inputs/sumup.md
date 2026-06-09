# SumUp Input

SumUp merchant, transaction, payout, and checkout data.

## Configuration

```yaml
data_sources:
  sumup:
    SumUp:
      merchant_code: MERCHANTCODE
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 7
      oauth_client_id: ${SUMUP_CLIENT_ID}
      oauth_client_secret: ${SUMUP_CLIENT_SECRET}
      oauth_refresh_token: ${SUMUP_REFRESH_TOKEN}
```

| Field | Default | Description |
| --- | --- | --- |
| `merchant_code` | *(required)* | SumUp merchant code |
| `start_date` | *(required)* | First sync date (`YYYY-MM-DD`) |
| `lookback_days` | `7` | Transaction lookback window |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `streams` | profile set | Explicit namespace list |
| `min_query_interval_ms` | `200` | Minimum delay between API calls |
| `oauth_token_url` | SumUp token endpoint | OAuth token URL |
| `access_token` | | Short-lived bearer token |
| `oauth_*` | | OAuth refresh credentials |
| `privacy` | | Field redaction settings |

## Pipeline wiring

```yaml
pipelines:
  payments:
    data_source: data_sources.sumup
    data_sink: data_sinks.landing
```
