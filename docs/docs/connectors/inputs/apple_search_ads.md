---
title: Apple Search Ads
description: Land daily Apple Search Ads reports in your warehouse using Apple API client credentials.
---

# Apple Search Ads

Reads daily Search Ads reports (campaigns, keywords, search terms). Apple uses client-credentials JWT, not a browser redirect. Pair with a destination that can rewrite a day: [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake).

## Before you begin

1. In Apple Search Ads, copy the **organization ID** (`org_id`).
2. Create an API client: `client_id`, `team_id`, `key_id`, and a `.p8` private key.
3. Store the key path in the environment.

```bash
export APPLE_SEARCH_ADS_CLIENT_ID="..."
export APPLE_SEARCH_ADS_TEAM_ID="..."
export APPLE_SEARCH_ADS_KEY_ID="..."
export APPLE_SEARCH_ADS_PRIVATE_KEY_PATH="/path/to/AuthKey.p8"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceAppleSearchAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "asa",
    DataSourceAppleSearchAds(
        org_id="123456",
        client_id=EnvRef("APPLE_SEARCH_ADS_CLIENT_ID"),
        team_id=EnvRef("APPLE_SEARCH_ADS_TEAM_ID"),
        key_id=EnvRef("APPLE_SEARCH_ADS_KEY_ID"),
        start_date="2024-01-01",
        private_key_path="${APPLE_SEARCH_ADS_PRIVATE_KEY_PATH}",
        stream_profile="full",
    ),
)
cfg.pipeline("asa", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source apple-search-ads \
  --pipeline asa \
  --name asa \
  --org-id 123456 \
  --client-id '${APPLE_SEARCH_ADS_CLIENT_ID}' \
  --team-id '${APPLE_SEARCH_ADS_TEAM_ID}' \
  --key-id '${APPLE_SEARCH_ADS_KEY_ID}' \
  --start-date 2024-01-01 \
  --private-key-path '${APPLE_SEARCH_ADS_PRIVATE_KEY_PATH}' \
  --stream-profile full
```

```yaml [YAML]
data_sources:
  asa:
    AppleSearchAds:
      org_id: "123456"
      client_id: ${APPLE_SEARCH_ADS_CLIENT_ID}
      team_id: ${APPLE_SEARCH_ADS_TEAM_ID}
      key_id: ${APPLE_SEARCH_ADS_KEY_ID}
      private_key_path: ${APPLE_SEARCH_ADS_PRIVATE_KEY_PATH}
      start_date: "2024-01-01"
      stream_profile: full
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `org_id` | string | Required | Organization ID (`X-AP-Context`) |
| `client_id` | string | Required | Apple API client ID |
| `team_id` | string | Required | Team ID for the client-secret JWT |
| `key_id` | string | Required | Key id for the `.p8` |
| `start_date` | string | Required | First report date |
| `private_key_path` | path | Not set | Path to the `.p8` |
| `private_key_pem` | secret | Not set | Key material instead of a path |
| `access_token` | secret | Not set | Static bearer for smoke tests |
| `end_date` | string | Yesterday minus lag | Last report date |
| `lookback_days` | integer | `3` | Days to re-fetch |
| `stream_profile` | string | `full` | `minimal`, `standard`, or `full` |
| `processing_lag_days` | integer | Not set | Skip immature trailing days |
| `time_zone` | string | Account default | Report timezone (`ORTZ` for search terms) |
| `return_records_with_no_metrics` | bool | `true` | Include zero-metric rows |
| `max_concurrent_requests` | integer | `8` | Parallel report requests |
| `streams` | list | Profile set | Exact stream list |

## What gets synced

Daily report grains in the selected profile. Each run rewrites recent days (`lookback_days`). See [How sources land](/concepts/source-landing-semantics).

## Troubleshooting

| Symptom | Fix |
|---|---|
| 401 | Check `client_id`, `team_id`, `key_id`, and that the `.p8` matches |
| Empty reports | Confirm `org_id` and that campaigns exist in the date window |
| Destination rejects the write | Use Athena, Athena Iceberg, or SkipprLake |

## Next steps

- [Google Ads](/connectors/inputs/google_ads)
- [How sources land](/concepts/source-landing-semantics)
