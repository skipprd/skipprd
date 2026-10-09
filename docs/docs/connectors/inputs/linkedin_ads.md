---
title: LinkedIn Ads
description: Land daily LinkedIn Campaign Manager reports in your warehouse.
---

# LinkedIn Ads

Reads daily Campaign Manager reports. Use a marketing-developer app and a refresh token with advertising reporting scopes. Pair with Athena, Athena Iceberg, or SkipprLake.

## Before you begin

1. Create an app at [LinkedIn Developer](https://www.linkedin.com/developers/) and enable Advertising.
2. Complete OAuth and copy the refresh token plus client id/secret.
3. Copy the ad account id from Campaign Manager.

```bash
export LINKEDIN_ADS_CLIENT_ID="..."
export LINKEDIN_ADS_CLIENT_SECRET="..."
export LINKEDIN_ADS_REFRESH_TOKEN="..."
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceLinkedInAds, EnvRef, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "li_ads",
    DataSourceLinkedInAds(
        ad_account_id="123456",
        start_date="2024-01-01",
        oauth_token_url="https://www.linkedin.com/oauth/v2/accessToken",
        oauth_client_id="${LINKEDIN_ADS_CLIENT_ID}",
        oauth_client_secret=EnvRef("LINKEDIN_ADS_CLIENT_SECRET"),
        oauth_refresh_token=EnvRef("LINKEDIN_ADS_REFRESH_TOKEN"),
        stream_profile="full",
    ),
)
cfg.pipeline("li", Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")))
cfg.save()
```

```bash [CLI]
skipprd connect data-source linkedin-ads \
  --pipeline li \
  --name li_ads \
  --ad-account-id 123456 \
  --start-date 2024-01-01 \
  --oauth-token-url https://www.linkedin.com/oauth/v2/accessToken \
  --oauth-client-id '${LINKEDIN_ADS_CLIENT_ID}' \
  --oauth-client-secret '${LINKEDIN_ADS_CLIENT_SECRET}' \
  --oauth-refresh-token '${LINKEDIN_ADS_REFRESH_TOKEN}' \
  --stream-profile full
```

```yaml [YAML]
data_sources:
  li_ads:
    LinkedInAds:
      ad_account_id: "123456"
      start_date: "2024-01-01"
      stream_profile: full
      oauth_token_url: https://www.linkedin.com/oauth/v2/accessToken
      oauth_client_id: ${LINKEDIN_ADS_CLIENT_ID}
      oauth_client_secret: ${LINKEDIN_ADS_CLIENT_SECRET}
      oauth_refresh_token: ${LINKEDIN_ADS_REFRESH_TOKEN}
```

:::

## Options

| Key | Type | Required/Default | Description |
|---|---|---|---|
| `ad_account_id` | string | Required | Campaign Manager account id |
| `start_date` | string | Required | First report date |
| `access_token` | secret | Not set | Static token (skips refresh) |
| `oauth_token_url` | URL | Not set | Token endpoint |
| `oauth_client_id` | string | Not set | App client id |
| `oauth_client_secret` | secret | Not set | App secret |
| `oauth_refresh_token` | secret | Not set | Refresh token |
| `rest_version` | string | Not set | REST version override |
| `api_version` | string | Not set | API version override |
| `end_date` | string | Not set | Last report date |
| `lookback_days` | integer | Not set | Days to re-fetch |
| `stream_profile` | string | Not set | `minimal`, `standard`, or `full` |
| `processing_lag_days` | integer | Not set | Skip immature days |
| `streams` | list | Profile set | Exact stream list |

## What gets synced

Daily campaign and creative reports in the selected profile. Each run rewrites recent days.

## Troubleshooting

| Symptom | Fix |
|---|---|
| 401 | Refresh the OAuth app and tokens |
| 403 | Enable Advertising products on the app and the user |
| Empty reports | Confirm `ad_account_id` and the date window |

## Next steps

- [Google Ads](/connectors/inputs/google_ads)
- [How sources land](/concepts/source-landing-semantics)
