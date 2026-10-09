---
title: HubSpot CRM
description: Sync HubSpot deals, companies, pipeline stages, forms, and CRM lifecycle events into your warehouse for funnel and revenue reporting.
---

# HubSpot CRM

Use the HubSpot CRM source to report on your sales funnel without copying contact details into the warehouse. Each run lands a daily snapshot of deals, companies, deal pipeline stages, forms, and landing pages, plus an event table of what changed: deals created and moving stage, contacts created and changing lifecycle stage, tickets opened and closed, and marketing emails published.

Contacts never land as a table. They appear only as IDs and lifecycle events, so the warehouse holds funnel data rather than a copy of your address book.

## Before you begin

1. **Create a private app in HubSpot.** Go to **Settings → Integrations → Private Apps → Create a private app**. On the **Scopes** tab, grant read access for the streams you plan to sync:

   | Stream | Scopes |
   |---|---|
   | `crm` | `crm.objects.deals.read`, `crm.objects.contacts.read`, `crm.objects.companies.read` |
   | `marketing` | `forms`, `content` |
   | `service` | `tickets` |
   | `onsite` | `content` |

   Create the app and copy its access token.
2. **Find your Hub ID.** Click your account name in the top-right corner of HubSpot. The Hub ID is the number shown under the account name.
3. **Pick a destination that supports how HubSpot lands.** The snapshot tables use replace-partition writes (each run rewrites that day's partition), and you can't change this. Use [Athena](/connectors/outputs/athena), [Athena Iceberg](/connectors/outputs/athenaiceberg), or [SkipprLake](/connectors/outputs/skipprlake).

## Configure

Export the token so the config can reference it without storing it:

```bash
export HUBSPOT_ACCESS_TOKEN="pat-na1-..."
```

Add the source and wire it into a pipeline. These examples assume you already have a destination named `lake`.

::: code-group

```python [Python]
from skippr import Config, DataSourceHubspotCrm, EnvRef, Pipeline

cfg = Config.discover()
hubspot = cfg.data_source(
    "hubspot",
    DataSourceHubspotCrm(
        hub_id="12345678",
        start_date="2024-01-01",
        access_token=EnvRef("HUBSPOT_ACCESS_TOKEN"),
    ),
)
cfg.pipeline(
    "crm",
    Pipeline(data_source=hubspot, data_sink=cfg.get_data_sink("lake")),
)
cfg.save()
```

```bash [CLI]
skipprd connect data-source hubspot-crm \
  --pipeline crm \
  --name hubspot \
  --hub-id 12345678 \
  --start-date 2024-01-01 \
  --access-token '${HUBSPOT_ACCESS_TOKEN}'
```

```yaml [YAML]
data_sources:
  hubspot:
    HubspotCrm:
      hub_id: "12345678"
      start_date: "2024-01-01"
      access_token: ${HUBSPOT_ACCESS_TOKEN}

pipelines:
  crm:
    data_source: data_sources.hubspot
    data_sink: data_sinks.lake
```

:::

With the CLI, connect the destination to the same pipeline with `skipprd connect data-sink … --pipeline crm`.

Check the connection and run one pass:

```bash
skipprd discover --pipeline crm
skipprd sync --pipeline crm --once
```

A successful run lands a row in `hubspot_sync_run_daily` with `status = ok` and the streams it synced.

### Use OAuth instead of a private app

If you use a HubSpot public app, leave `access_token` unset and provide the refresh credentials. Skipprd exchanges the refresh token for an access token at the start of each run.

```yaml
data_sources:
  hubspot:
    HubspotCrm:
      hub_id: "12345678"
      start_date: "2024-01-01"
      oauth_token_url: https://api.hubapi.com/oauth/v1/token
      oauth_client_id: 00000000-0000-0000-0000-000000000000
      oauth_client_secret: ${HUBSPOT_CLIENT_SECRET}
      oauth_refresh_token: ${HUBSPOT_REFRESH_TOKEN}
```

## Options

| Key | Type | Default | Description |
|---|---|---|---|
| `hub_id` | string | Required | Your HubSpot Hub ID. Written to every row as `hub_id`. Quote it in YAML. |
| `start_date` | string (`YYYY-MM-DD`) | Required | Required by the config. HubSpot reads currently ignore it: each run reads current object state. |
| `lookback_days` | integer | `7` | Accepted but not applied in this version. |
| `stream_profile` | string | `console_default` | Which streams to sync: `minimal`, `console_default`, or `full`. See [What gets synced](#what-gets-synced). |
| `streams` | list of strings | Not set | Sync exactly these streams instead of a profile: `crm`, `marketing`, `service`, `onsite`. Unknown names are ignored. |
| `min_query_interval_ms` | integer | `200` | Accepted but not applied in this version. |
| `access_token` | secret | Not set | Private app access token, as `${ENV_VAR}`. When set, OAuth settings are ignored. |
| `oauth_token_url` | string | Not set | OAuth token endpoint. Required when `access_token` is not set. |
| `oauth_client_id` | string | Not set | OAuth client ID. |
| `oauth_client_secret` | secret | Not set | OAuth client secret, as `${ENV_VAR}`. |
| `oauth_refresh_token` | secret | Not set | OAuth refresh token, as `${ENV_VAR}`. |
| `privacy.mode` | string | `passthrough` | `passthrough`, `profile`, or `allowlist`. See [Personal data](#personal-data). |
| `privacy.profile` | string | `passthrough` | `passthrough`, `upfoundry_safe`, or `crm_revenue_only`. The two non-passthrough profiles behave the same today. Setting either switches on profile mode even when `mode` is `passthrough`. |
| `privacy.drop_properties` | list of strings | `[]` | Extra field names to remove (case-insensitive) when a non-passthrough profile is active. |
| `privacy.keep_properties` | list of strings | `[]` | In `allowlist` mode, the only top-level fields kept on each row. |
| `privacy.drop_streams` | list of strings | `[]` | Accepted but not applied in this version. Use `streams` to skip a stream. |
| `privacy.hash_properties` | list of strings | `[]` | Accepted but not applied in this version. |
| `privacy.on_violation` | string | `drop` | `drop` or `deadletter`. Accepted but not applied in this version. |

## What gets synced

| Stream | Tables | `minimal` | `console_default` | `full` |
|---|---|:-:|:-:|:-:|
| `crm` | `hubspot_portal_snapshot`, `hubspot_pipeline_stage_dim`, `hubspot_deal_snapshot`, `hubspot_company_snapshot`, plus deal, contact, and company events | ✓ | ✓ | ✓ |
| `marketing` | `hubspot_form_dim`, plus marketing-email events | | ✓ | ✓ |
| `service` | Ticket events | | ✓ | ✓ |
| `onsite` | `hubspot_landing_page_dim` | | | ✓ |

All events land in one table, `hubspot_event_fact`, with an `event_type` column:

| `event_type` | Comes from |
|---|---|
| `deal.created`, `deal.stage_changed` | Deal property history |
| `contact.created`, `contact.lifecycle_changed` | Contact property history |
| `company.created` | Company create date |
| `ticket.created`, `ticket.closed` | Tickets |
| `email.published` | Marketing emails |

Every run also writes one row to `hubspot_sync_run_daily` with `run_date`, `status`, `streams_synced`, and `elapsed_ms`.

**Snapshots.** The snapshot and dimension tables carry a `run_date` column (the UTC date of the run). Each run replaces that day's rows, so re-running on the same day is safe and each day keeps its own copy. To get current state, filter to the latest `run_date`. Deal snapshots include amount, stage, pipeline, close date, owner, and UTM attribution fields.

**Events.** `hubspot_event_fact` is incremental. Skipprd remembers the newest `occurred_at` it has landed and, on the next run, adds only events after it. Events are appended and keyed by `event_id`.

**Volume limits.** Each run reads the first 100 deals, contacts, companies, and tickets that HubSpot's search returns, and the first 50 landing pages and marketing emails. Larger portals are only partly covered in this version.

**Deletes.** A deal or company you delete stops appearing in new snapshots. Earlier days keep it. Events already landed are kept.

**Discover.** `skipprd discover` reads only the `crm` stream to learn column types.

### Personal data

Contacts land only as IDs and lifecycle events. The default `passthrough` mode also removes any top-level field whose name is exactly a personal-data term such as `email`, `phone`, `firstname`, `lastname`, `address`, `city`, `state`, `zip`, or `country`. That is why the company `country` and the landing page `state` columns don't land by default.

| Setting | Effect |
|---|---|
| `mode: passthrough` (default) | Removes top-level fields named exactly like personal data. |
| `profile: upfoundry_safe` or `crm_revenue_only` | Also removes any field, at any depth, whose name *contains* a personal-data term (such as `email`, `content`, `note`, or `subject`), plus every field in `drop_properties`. Deal and contact reads also limit themselves to a fixed list of revenue and attribution properties. |
| `mode: allowlist` | Keeps only the top-level fields in `keep_properties`. Always include each table's ID column and `run_date`. |

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `sink '…' does not support write policy ReplacePartition for namespace 'hubspot_…'` | Your destination only appends. | Use Athena, Athena Iceberg, or SkipprLake. |
| `HubSpot requires oauth_refresh_token + client credentials …` | No token reached Skipprd, so it fell back to OAuth with no token URL. | Export the variable named in `access_token` in the shell that runs `skipprd`, and check the name matches. |
| `HubSpot HTTP 401` | The token is wrong or was rotated. | Copy the current token from the private app and update the variable. |
| `HubSpot HTTP 403` | The app is missing a scope a selected stream needs. | Add the scope from [Before you begin](#before-you-begin), or remove that stream from `streams`. |
| `HubSpot HTTP 429` | Your portal's API rate limit was reached. | Run less often, or sync fewer streams. |
| Only `hubspot_sync_run_daily` lands | Every name in `streams` is misspelled; unknown names are ignored. | Use `crm`, `marketing`, `service`, or `onsite`. |
| Fewer deals than in HubSpot | Each run reads at most 100 records per object type. | This is a current limit. |
| No new events after the first run | Nothing changed after the newest event already landed. | This is expected. New stage and lifecycle changes appear on the next run. |

## Next steps

- [How sources land](/concepts/source-landing-semantics)
- [SkipprLake destination](/connectors/outputs/skipprlake)
- [skipprd sync](/cli/sync)
