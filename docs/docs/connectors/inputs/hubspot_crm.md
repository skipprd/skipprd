# HubSpot CRM Input

HubSpot CRM objects — companies, deals, contacts, forms, and marketing events.

## Configuration

```yaml
data_sources:
  hubspot:
    HubspotCrm:
      hub_id: "12345678"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 7
      access_token: ${HUBSPOT_ACCESS_TOKEN}
```

| Field | Default | Description |
| --- | --- | --- |
| `hub_id` | *(required)* | HubSpot portal (hub) ID |
| `start_date` | *(required)* | First sync date (`YYYY-MM-DD`) |
| `lookback_days` | `7` | Event and engagement lookback |
| `stream_profile` | `full` | `minimal`, `standard`, or `full` |
| `streams` | profile set | Explicit namespace list |
| `min_query_interval_ms` | `200` | Minimum delay between API calls |
| `access_token` | | Private app or OAuth access token |
| `oauth_*` | | OAuth refresh credentials |
| `privacy` | | Field redaction settings |

## Pipeline wiring

```yaml
pipelines:
  crm:
    data_source: data_sources.hubspot
    data_sink: data_sinks.landing
```

Snapshot namespaces (companies, deals) and fact namespaces (events) use incremental checkpoints keyed on `occurred_at` or object update times.
