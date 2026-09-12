# Google Analytics (GA4) Input

Curated **daily fact grains** from the GA4 Data API (`runReport`). One bronze namespace per stable dimension set; warehouse SQL builds rollups and reports.

Not the GA4 BigQuery event export. See [GA4 bronze & modeling](../../concepts/ga4-bronze-and-modeling.md).

## Configuration

```yaml
data_sources:
  ga4:
    GoogleAnalytics:
      property_id: "123456789"
      start_date: "2024-01-01"
      stream_profile: full
      lookback_days: 7
      processing_lag_days: 1
      window_in_days: 1
      keep_empty_rows: true
      access_token: ${GA4_ACCESS_TOKEN}
```

| Variable | Default | Description |
| --- | --- | --- |
| `property_id` | *(required)* | GA4 property ID |
| `start_date` | *(required)* | First date (`YYYY-MM-DD`) |
| `end_date` | | Last date; omit for through yesterday minus lag |
| `stream_profile` | `full` | `minimal` (4), `standard` (16), `full` (23) |
| `streams` | profile set | Explicit namespace list; overrides profile |
| `lookback_days` | `3` | Mature days to re-fetch |
| `processing_lag_days` | `1` | Skip last N calendar days |
| `window_in_days` | `1` | Days per API date range; >1 risks sampling |
| `keep_empty_rows` | `true` | Zero-metric dimension rows in API response |
| Auth fields | | `access_token`, OAuth refresh, or service account |

### Accuracy knobs

| Setting | Purpose |
| --- | --- |
| `replace_partition` + `lookback_days` | Correct revised mature days |
| `processing_lag_days` | Avoid immature trailing days |
| `window_in_days: 1` | Minimize sampling (default) |

### Stream profiles

- **full** — 23 namespaces (production default)
- **standard** — 16 (no demographics, ecommerce, publisher ads)
- **minimal** — 4 acquisition/event tables (dev/CI)

### Namespace catalog

See the full table in [GA4 bronze & modeling](../../concepts/ga4-bronze-and-modeling.md).

Ecommerce and publisher-ads namespaces are **optional**: invalid dimension/metric errors skip that stream for the run.

### Landing semantics

[Source landing semantics](../../concepts/source-landing-semantics.md). Pair with [Athena](../outputs/athena.md).

## Authentication

Skipprd calls the [Google Analytics Data API](https://developers.google.com/analytics/devguides/reporting/data/v1) with a **Bearer** token. Required OAuth scope:

`https://www.googleapis.com/auth/analytics.readonly`

The Google account or service account must have **Viewer** (or higher) on the GA4 property ([Admin → Property access management](https://support.google.com/analytics/answer/9305788)).

Pick **one** method below. The plugin checks credentials in this order: `access_token` → OAuth refresh → service account → `GOOGLE_APPLICATION_CREDENTIALS`.

Both production methods below share the same **Google Cloud project** prerequisite.

#### Prerequisite: Google Cloud project + Data API

1. Open [Google Cloud Console](https://console.cloud.google.com/) and select (or create) a project.
2. Go to **APIs & Services → Library**, search for **Google Analytics Data API**, and click **Enable**.
3. Note your **GA4 property ID** (GA4 **Admin → Property settings** → Property ID, numeric only — no `properties/` prefix).

---

### Service account (recommended for production) {#service-account}

Best for CI, servers, and scheduled `skipprd sync`. Skipprd reads the JSON key and mints short-lived access tokens automatically. You do **not** set `GA4_ACCESS_TOKEN`.

#### 1. Create the service account (Cloud Console)

1. **IAM & Admin → Service Accounts → Create service account**.
2. Name it (for example `skippr-ga4-read`) and click **Create and continue**.
3. **Grant this service account access to project** — optional for GA4 Data API reads; property-level access in GA4 (next step) is what matters. Click **Done**.
4. Open the new service account → **Keys → Add key → Create new key → JSON** → **Create**. Store the downloaded `.json` file securely (treat it like a password).

The key file contains a field `client_email`, for example `skippr-ga4-read@my-project.iam.gserviceaccount.com`. You need that email in GA4.

#### 2. Grant the service account access to the GA4 property

Service accounts do not inherit your personal Google login. Add the robot account explicitly:

1. In [Google Analytics](https://analytics.google.com/), open the target property.
2. **Admin** (gear) → **Property access management**.
3. **+** → **Add users**.
4. Paste the service account **email** from the JSON (`client_email`), for example `skipprd@optimistic-jet-274810.iam.gserviceaccount.com`.
5. Role: **Viewer** (minimum for read-only Data API). Turn off **Notify new users by email** (there is no inbox for a service account). Save.

Without this step, sync fails with **403** even if the key file is valid.

#### 2a. UI error: “This email doesn’t match a Google Account”

The GA4 **Add users** dialog only validates **human** Google accounts (`@gmail.com`, Google Workspace). Many properties show an error for `*.iam.gserviceaccount.com` even though the address is correct. That is a [known GA4 UI limitation](https://stackoverflow.com/questions/78464689/ga-property-access-management-can-t-add-service-account), not a problem with your service account.

**Fastest workaround — use [OAuth refresh](#oauth-refresh) instead:** authorize with the same Google account that already has GA4 **Administrator** access. Skipprd does not need the service account on the property for that path.

**If you must use the service account**, grant access with the **Google Analytics Admin API** (as a GA4 admin user):

1. In Cloud Console, enable **Google Analytics Admin API** (same project as the service account).
2. Authenticate as a user who is **Administrator** on the GA4 property:

```bash
gcloud auth application-default login \
  --scopes=https://www.googleapis.com/auth/analytics.manage.users,https://www.googleapis.com/auth/cloud-platform
```

3. Create a property access binding (replace `PROPERTY_ID` and the service account email):

```bash
export PROPERTY_ID="123456789"   # numeric GA4 property ID
export SA_EMAIL="skipprd@optimistic-jet-274810.iam.gserviceaccount.com"

curl -s -X POST \
  "https://analyticsadmin.googleapis.com/v1alpha/properties/${PROPERTY_ID}/accessBindings" \
  -H "Authorization: Bearer $(gcloud auth application-default print-access-token)" \
  -H "Content-Type: application/json" \
  -d "{\"user\": \"${SA_EMAIL}\", \"roles\": [\"predefinedRoles/viewer\"]}"
```

4. In GA4 **Property access management**, confirm the service account appears in the user list (it may show up after the API call even when the UI refused manual entry).

If the API returns an error, use OAuth refresh for now or ask a GA4 **account** administrator to run the same `curl` at account level (`parent=accounts/ACCOUNT_ID`).

#### 3. Configure Skipprd

**Option A — path in config** (explicit, works everywhere):

```yaml
# skippr.yml (engine / runtime plugin)
data_sources:
  ga4:
    GoogleAnalytics:
      property_id: "123456789"
      start_date: "2024-01-01"
      service_account_json_path: /secure/path/skippr-ga4-key.json
```

**Option B — environment variable** (common in containers):

```bash
export GOOGLE_APPLICATION_CREDENTIALS="/secure/path/skippr-ga4-key.json"
```

Omit `service_account_json_path` when using `GOOGLE_APPLICATION_CREDENTIALS`; Skipprd picks up the path automatically.


Do **not** set `access_token` when using a service account.

#### 4. Verify

```bash
skipprd sync
```

If auth fails, confirm the Data API is enabled, the JSON path is readable, and the service account email appears under property access management.

---

### OAuth refresh (user-delegated automation) {#oauth-refresh}

Use when a **human Google account** should own access (not a robot account), for example a workspace user who already has GA4 access. Skipprd stores a **refresh token** and requests a new access token on each sync. You do **not** set `GA4_ACCESS_TOKEN`.

All four OAuth fields must be set for Skipprd to use this path: `oauth_token_url`, `oauth_client_id`, `oauth_client_secret`, `oauth_refresh_token`.

#### 1. Configure the OAuth consent screen

1. Cloud Console → **APIs & Services → OAuth consent screen**.
2. User type:
   - **Internal** — only if you use **Google Workspace** and the GA4 property is in the same org. No Google verification required for coworkers.
   - **External** — personal Gmail or mixed accounts. For your own testing, leave publishing status as **Testing** (do not click **Publish app**).
3. Fill required app name and support email.
4. **Scopes → Add or remove scopes** → add manually:
   `https://www.googleapis.com/auth/analytics.readonly`  
   Google may show **“needs verification”** for External apps — that applies only if you publish to the public. In **Testing** mode you can use the scope without verification for accounts listed as test users.
5. **Test users** (required for External + Testing): add every Google account that will run Playground / `skipprd sync` (your `@gmail.com` or Workspace email).
6. Save. You do **not** need Google’s app review for a private Skipprd pipeline while status stays **Testing** and you are a listed test user.

#### 2. Create an OAuth client ID

1. **APIs & Services → Credentials → Create credentials → OAuth client ID**.
2. Application type: **Web application** (works with the OAuth Playground below).
3. Name it (for example `skippr-ga4-oauth`).
4. **Authorized redirect URIs** → add:

   `https://developers.google.com/oauthplayground`

5. **Create** → copy the **Client ID** and **Client secret**.

#### 3. Obtain a refresh token (one-time)

Use the [OAuth 2.0 Playground](https://developers.google.com/oauthplayground/) with your client:

1. Click the **gear** icon (top right) → check **Use your own OAuth credentials** → paste Client ID and Client secret → **Close**.
2. In the left panel, scroll to **Google Analytics Data API v1** or paste into the input box:
   `https://www.googleapis.com/auth/analytics.readonly`
3. Click **Authorize APIs** → sign in with the **same Google user** that has Viewer access on the GA4 property.
4. Click **Exchange authorization code for tokens**.
5. Copy the **Refresh token** from the response. Store it in a secret manager or env var — it does not expire unless revoked.

If no refresh token appears, revoke prior access at [Google Account permissions](https://myaccount.google.com/permissions), then repeat with the gear menu option **Force prompt** (if shown) so Google issues a new refresh token.

#### 4. Configure Skipprd

Set secrets via environment variables (recommended):

```bash
export GA4_OAUTH_CLIENT_ID="....apps.googleusercontent.com"
export GA4_OAUTH_CLIENT_SECRET="...."
export GA4_OAUTH_REFRESH_TOKEN="1//...."
```

```yaml
# skippr.yml
data_sources:
  ga4:
    GoogleAnalytics:
      property_id: "123456789"
      start_date: "2024-01-01"
      oauth_token_url: https://oauth2.googleapis.com/token
      oauth_client_id: ${GA4_OAUTH_CLIENT_ID}
      oauth_client_secret: ${GA4_OAUTH_CLIENT_SECRET}
      oauth_refresh_token: ${GA4_OAUTH_REFRESH_TOKEN}
```

`oauth_token_url` must be `https://oauth2.googleapis.com/token` for standard Google OAuth clients.

> Do not open this URL in a browser
`oauth2.googleapis.com/token` is a **POST-only API endpoint**. Opening it in Chrome/Safari shows “page can’t be found” — that is normal. Put the URL in `skippr.yml` only; Skipprd (or `curl` below) sends the refresh token there in the background.


Do **not** set `access_token` when using refresh credentials.

#### 5. Verify

Optional — confirm the refresh token works:

```bash
curl -s -X POST https://oauth2.googleapis.com/token \
  -d "client_id=${GA4_OAUTH_CLIENT_ID}" \
  -d "client_secret=${GA4_OAUTH_CLIENT_SECRET}" \
  -d "refresh_token=${GA4_OAUTH_REFRESH_TOKEN}" \
  -d "grant_type=refresh_token" | jq -r .access_token
```

Then run `skipprd discover` then `skipprd sync --once`. On **401**, the refresh token may have been revoked; repeat section 3.

#### Service account vs OAuth refresh

| | Service account | OAuth refresh |
| --- | --- | --- |
| Identity | Robot (`...@....iam.gserviceaccount.com`) | Human Google user |
| GA4 access | Add SA email in property access management | User must already have property access |
| Secrets | JSON key file | Client ID, secret, refresh token |
| Typical use | Production ETL, CI, VMs | When policy blocks service accounts on GA4 |

---

### Bearer access token (`GA4_ACCESS_TOKEN`)

`GA4_ACCESS_TOKEN` is the **OAuth 2.0 access token** string (starts with `ya29.` for Google). It is **not** a separate API key from the GA4 UI, and it **expires** (typically after about one hour). Use this for local testing; prefer a service account or refresh token for automation.

#### Option A — OAuth 2.0 Playground (quickest for a first sync)

1. Enable the **Google Analytics Data API** and create an **OAuth client ID** in Cloud Console (same as refresh flow above).
2. Open [OAuth 2.0 Playground](https://developers.google.com/oauthplayground/).
3. Gear icon → **Use your own OAuth credentials** → paste client ID and secret.
4. Step 1: input scope `https://www.googleapis.com/auth/analytics.readonly` → **Authorize APIs** → sign in with a user that has access to the property.
5. Step 2: **Exchange authorization code for tokens** → copy **Access token**.
6. Export and run:

```bash
export GA4_ACCESS_TOKEN="ya29...."   # paste access token from Step 2
skipprd sync
```

Your config can reference the env var:

```yaml
access_token: ${GA4_ACCESS_TOKEN}
```

Set `access_token: ${GA4_ACCESS_TOKEN}` in `skippr.yml`.

#### Option B — gcloud (if you already use Google Cloud CLI)

Log in with the Analytics scope, then print an access token:

```bash
gcloud auth application-default login \
  --scopes=https://www.googleapis.com/auth/analytics.readonly,https://www.googleapis.com/auth/cloud-platform

export GA4_ACCESS_TOKEN="$(gcloud auth application-default print-access-token)"
```

The token expires; re-run `print-access-token` when sync returns **401**.

#### Option C — curl (when you already have a refresh token)

```bash
curl -s -X POST https://oauth2.googleapis.com/token \
  -d client_id="${GA4_OAUTH_CLIENT_ID}" \
  -d client_secret="${GA4_OAUTH_CLIENT_SECRET}" \
  -d refresh_token="${GA4_OAUTH_REFRESH_TOKEN}" \
  -d grant_type=refresh_token \
  | jq -r .access_token
```

Set `GA4_ACCESS_TOKEN` to the printed value, or configure the refresh fields in `skippr.yml` so Skipprd refreshes automatically (preferred).

### Security

- Do not commit tokens, refresh tokens, or JSON keys in git. Use `${ENV_VAR}` in config.
- Rotate compromised credentials in Cloud Console and GA4 property access.



## Development fixtures

`SKIPPR_GA4_FIXTURE_DIR` — JSON files `{namespace_with_dots_as_underscores}_{YYYYMMDD}.json` (example: `google_analytics_events_daily_20240101.json`). See `plugins/data_source/google_analytics/tests/fixtures/`.

## Troubleshooting

| Symptom | Fix |
| --- | --- |
| **This email doesn’t match a Google Account** (service account) | GA4 UI cannot add `*.iam.gserviceaccount.com` on many properties — use [OAuth refresh](#oauth-refresh) or [Admin API access binding](#2a-ui-error-this-email-doesnt-match-a-google-account) |
| 401 / 403 | Regenerate `GA4_ACCESS_TOKEN` (expired), confirm `analytics.readonly` scope, and property **Viewer** access (or valid access binding for the service account) |
| `GA4 requires access_token...` | Set `GA4_ACCESS_TOKEN`, OAuth refresh fields, or service account path — see [Authentication](#authentication) |
| Empty ecommerce/ads tables | Enable features in GA4; streams are optional and may be skipped |
| Stale metrics | Confirm `replace_partition`; increase `lookback_days` |
| Sampling concerns | Keep `window_in_days: 1` |
| OAuth scope **needs verification** / can’t publish app | Keep consent screen in **Testing**; add yourself under **Test users**; do not publish. Or use **Internal** if on Workspace. |
| **`gcloud auth application-default login`: This app is blocked** | Workspace admin may block third-party Google sign-in, or the wrong account is signed in. Skip `gcloud` for Skipprd OAuth refresh — it is not required. Use Playground + env vars, or a service account JSON key. |
| Playground / OAuth **access blocked** | Same as above: Testing + test users; try a personal Gmail GCP project; ask Workspace admin to allow your OAuth app or “Google Cloud SDK”. |

Offline dev: set `SKIPPR_GA4_FIXTURE_DIR` to JSON fixtures named `{namespace_with_underscores}_{YYYYMMDD}.json`.
