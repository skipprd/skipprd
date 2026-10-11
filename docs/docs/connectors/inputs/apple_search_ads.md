---
title: Apple Search Ads
description: Sync daily Apple Search Ads campaign, ad group, keyword, and search term performance into your warehouse with Skipprd.
---

# Apple Search Ads

The Apple Search Ads source reads daily reports from the Apple Search Ads API at campaign, ad group, keyword, and search term grain: impressions, taps, installs, and spend. Use it to bring App Store ad performance into your warehouse next to your other acquisition channels and in-app revenue.

## Before you begin

Apple Search Ads uses OAuth client credentials signed with your own key pair; there is no browser sign-in. You need an API user, a key pair, three IDs from Apple, your organization ID, and a destination that can rewrite daily partitions.

1. **Create an API user.** In [Apple Search Ads](https://app.searchads.apple.com/), an account administrator opens **Account Settings → User Management** and invites a user with an **API** role, such as **API Account Read Only**. Accept the invitation and sign in as that user.
2. **Generate a key pair** on your machine:

   ```bash
   openssl ecparam -genkey -name prime256v1 -noout -out asa-private-key.pem
   openssl ec -in asa-private-key.pem -pubout -out asa-public-key.pem
   openssl pkcs8 -topk8 -nocrypt -in asa-private-key.pem -out asa-private-key-pkcs8.pem
   ```

   Keep `asa-private-key-pkcs8.pem` secret. That is the file Skipprd reads.
3. **Upload the public key.** As the API user, open **Account Settings → API**, paste the contents of `asa-public-key.pem`, and save. Apple then shows a **client ID**, **team ID**, and **key ID**. Copy all three.
4. **Find the organization ID.** It is the numeric ID shown next to your organization name in the account menu. Skipprd sends it on every request.
5. **Set up a destination.** Every stream is rewritten one day at a time, so the destination must support partition replacement: [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake). The examples below assume a data sink named `lake` already exists in `skippr.yml`.

```bash
export APPLE_SEARCH_ADS_CLIENT_ID="SEARCHADS.your-client-id"
export APPLE_SEARCH_ADS_TEAM_ID="your-team-id"
export APPLE_SEARCH_ADS_KEY_ID="your-key-id"
export APPLE_SEARCH_ADS_PRIVATE_KEY_PATH="/secure/path/asa-private-key-pkcs8.pem"
```

## Configure

::: code-group

```python [Python]
from skippr import Config, DataSourceAppleSearchAds, Pipeline

cfg = Config.discover()
src = cfg.data_source(
    "apple_search_ads",
    DataSourceAppleSearchAds(
        org_id="1234567",
        client_id="${APPLE_SEARCH_ADS_CLIENT_ID}",
        team_id="${APPLE_SEARCH_ADS_TEAM_ID}",
        key_id="${APPLE_SEARCH_ADS_KEY_ID}",
        private_key_path="${APPLE_SEARCH_ADS_PRIVATE_KEY_PATH}",
        start_date="2026-01-01",
    ),
)
cfg.pipeline(
    "apple_search_ads",
    Pipeline(data_source=src, data_sink=cfg.get_data_sink("lake")),
)
cfg.save()
```

```bash [CLI]
skipprd connect data-source apple-search-ads \
  --pipeline apple_search_ads \
  --name apple_search_ads \
  --org-id 1234567 \
  --client-id '${APPLE_SEARCH_ADS_CLIENT_ID}' \
  --team-id '${APPLE_SEARCH_ADS_TEAM_ID}' \
  --key-id '${APPLE_SEARCH_ADS_KEY_ID}' \
  --private-key-path '${APPLE_SEARCH_ADS_PRIVATE_KEY_PATH}' \
  --start-date 2026-01-01
```

```yaml [YAML]
pipelines:
  apple_search_ads:
    data_source: data_sources.apple_search_ads
    data_sink: data_sinks.lake

data_sources:
  apple_search_ads:
    AppleSearchAds:
      org_id: "1234567"
      client_id: ${APPLE_SEARCH_ADS_CLIENT_ID}
      team_id: ${APPLE_SEARCH_ADS_TEAM_ID}
      key_id: ${APPLE_SEARCH_ADS_KEY_ID}
      private_key_path: ${APPLE_SEARCH_ADS_PRIVATE_KEY_PATH}
      start_date: "2026-01-01"
```

:::

To keep the key out of the filesystem, set `private_key_pem: ${APPLE_SEARCH_ADS_PRIVATE_KEY}` with the PEM contents in that variable instead of `private_key_path`. Skipprd signs a short-lived token with the key, exchanges it with Apple for an access token, and renews it automatically.

With the CLI, wire the `lake` destination into the same pipeline with `skipprd connect data-sink … --pipeline apple_search_ads` if it is not there already.

### Check it worked

```bash
skipprd discover --pipeline apple_search_ads
skipprd sync --pipeline apple_search_ads --once
```

Then query `apple_search_ads_campaign_daily` in your destination. You should see one row per campaign per day from `start_date` through yesterday.

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `org_id` | string | Required | Apple Search Ads organization ID. |
| `client_id` | string | Required | Client ID shown after you upload the public key. |
| `team_id` | string | Required | Team ID shown after you upload the public key. |
| `key_id` | string | Required | Key ID shown after you upload the public key. |
| `start_date` | string | Required | First report day, `YYYY-MM-DD`. |
| `private_key_path` | string | — | Path to the PKCS#8 PEM private key. |
| `private_key_pem` | secret | — | The PEM private key itself. Used before `private_key_path` when both are set. |
| `access_token` | secret | — | A ready-made Apple access token. Skips the key exchange; it expires after about an hour, so use it only for tests. |
| `end_date` | string | — | Last report day, `YYYY-MM-DD`. Without it, Skipprd syncs through yesterday. |
| `lookback_days` | integer | `3` | Days before the last completed day to fetch again on every sync. |
| `processing_lag_days` | integer | `1` | Never sync days more recent than this many days ago. |
| `time_zone` | string | `UTC` | Time zone for report days. `ORTZ` uses the organization's time zone. `search_term_daily` always uses `ORTZ`, because Apple requires it for search term reports. |
| `return_records_with_no_metrics` | boolean | `true` | Include campaigns, ad groups, and keywords that had no activity that day. |
| `max_concurrent_requests` | integer | `8` | Report requests in flight at once for the ad group, keyword, and search term streams. |
| `stream_profile` | string | `full` | Which streams to sync: `minimal`, `standard`, or `full`. |
| `streams` | list of strings | — | Exact stream names to sync. Overrides `stream_profile`. |

Secret fields only accept `${NAME}` environment references (`EnvRef("NAME")` in Python).

## What gets synced

Each stream lands as its own table named after the stream, with the dot replaced by an underscore.

| Stream | Table | One row per | Profiles |
|---|---|---|---|
| `apple_search_ads.campaign_daily` | `apple_search_ads_campaign_daily` | campaign per day | minimal, standard, full |
| `apple_search_ads.ad_group_daily` | `apple_search_ads_ad_group_daily` | ad group per day | standard, full |
| `apple_search_ads.keyword_daily` | `apple_search_ads_keyword_daily` | keyword per day | full |
| `apple_search_ads.search_term_daily` | `apple_search_ads_search_term_daily` | search term per ad group per day | full |

Every row has `org_id`, `date`, `campaign_id`, and `campaign_name`. Ad group, keyword, and search term rows add `ad_group_id` and `ad_group_name`; keyword rows add `keyword_id` and `keyword`; search term rows add `search_term`. Metrics keep Apple's names (for example `impressions`, `taps`, and install counts), and `spend` is Apple's `localSpend` amount in the account currency.

**How requests fan out.** `campaign_daily` is one request per day. For the other streams, Skipprd first lists your campaigns (and, for keywords, every ad group), then requests one report per campaign or ad group per day, up to `max_concurrent_requests` at a time. Accounts with many ad groups make many requests for `keyword_daily`.

**Incremental window.** The first sync reads every day from `start_date` to yesterday. Each later sync starts again at the last completed day minus `lookback_days` and rewrites those days in full, so Apple's late install attribution replaces earlier numbers.

**Discover.** `skipprd discover` reads only `campaign_daily` for the last 3 days and does not save progress.

**Rate limits.** Skipprd retries rate-limit (429) and server (5xx) responses with exponential backoff, up to 8 attempts per request. It honours `Retry-After`, waits 30 seconds on a 429 without one, and never waits more than 2 minutes between attempts. Lower `max_concurrent_requests` if you keep hitting 429s.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Apple Search Ads requires private_key_path, private_key_pem, APPLE_SEARCH_ADS_PRIVATE_KEY_PATH, or access_token` | No private key is configured, or the path variable is empty. | Set `private_key_path` or `private_key_pem`. |
| `client_id, team_id, and key_id are required for live OAuth` | One of the three IDs is empty. | Copy all three from **Account Settings → API**. |
| Key parsing or signing error | The key is in SEC1 format (`BEGIN EC PRIVATE KEY`) rather than PKCS#8. | Convert it with the `openssl pkcs8 -topk8 -nocrypt` command in [Before you begin](#before-you-begin). |
| HTTP 401 or `invalid_client` from Apple | The IDs do not match the uploaded public key, or the key was replaced in Apple Search Ads. | Re-copy the client, team, and key IDs, or upload a new public key and update all three. |
| HTTP 403 | The API user has no access to `org_id`. | Check the organization ID and the user's role in **User Management**. |
| Ad group, keyword, or search term tables are empty | No campaigns were listed for the organization. | Confirm campaigns exist under `org_id`. |
| Search term rows don't line up with campaign rows by day | Search term reports always use the organization time zone. | Set `time_zone: ORTZ` so every stream uses the same days. |
| `sink '…' does not support write policy ReplacePartition` | The destination cannot rewrite daily partitions. | Use Athena, Athena Iceberg, or SkipprLake as the destination. |

## Next steps

- [How sources land](/concepts/source-landing-semantics) explains partition replacement and lookback.
- [Google Ads](/connectors/inputs/google_ads) and [Meta Ads](/connectors/inputs/meta_ads) add your other paid channels.
- [skipprd sync](/cli/sync) covers scheduling and continuous mode.
