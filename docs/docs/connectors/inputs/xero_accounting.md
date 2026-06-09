# Xero Accounting Input

Xero accounting entities — invoices, payments, bank transactions, contacts, and chart of accounts.

## Configuration

```yaml
data_sources:
  xero:
    XeroAccounting:
      tenant_id: "00000000-0000-0000-0000-000000000000"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 90
      oauth_client_id: ${XERO_CLIENT_ID}
      oauth_client_secret: ${XERO_CLIENT_SECRET}
      oauth_refresh_token: ${XERO_REFRESH_TOKEN}
```

| Field | Default | Description |
| --- | --- | --- |
| `tenant_id` | *(required)* | Xero organisation tenant ID |
| `start_date` | *(required)* | First sync date (`YYYY-MM-DD`) |
| `lookback_days` | `90` | Modified-since lookback window |
| `page_size` | `100` | API page size (1–100) |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `streams` | profile set | Explicit namespace list |
| `min_query_interval_ms` | `350` | Minimum delay between API calls |
| `oauth_token_url` | Xero token endpoint | OAuth token URL |
| `access_token` | | Short-lived bearer token |
| `oauth_*` | | OAuth refresh credentials |
| `privacy` | | Field redaction settings |

## Pipeline wiring

```yaml
pipelines:
  accounting:
    data_source: data_sources.xero
    data_sink: data_sinks.landing
```

Incremental sync uses `UpdatedDateUTC` checkpoints per stream.
