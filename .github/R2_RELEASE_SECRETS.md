# GitHub Actions secrets for Cloudflare R2 install CDN

Add these repository secrets on **skipprd/skipprd-private** (Settings → Secrets and variables → Actions).
Also add them to the **Skippr** environment if that environment overrides secret scope for publish jobs.

| Secret | Value | Notes |
|--------|-------|-------|
| `R2_ACCOUNT_ID` | Cloudflare account ID | Same as `CLOUDFLARE_ACCOUNT_ID` in `cloud/.env` |
| `R2_ACCESS_KEY_ID` | R2 S3 API access key | Same as `OBJECTS_ACCESS_KEY_ID` |
| `R2_SECRET_ACCESS_KEY` | R2 S3 API secret | Same as `OBJECTS_SECRET_ACCESS_KEY` |

Endpoint used by CI: `https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com`  
Bucket: `skippr-web-install` (public via Worker `install.skippr.io`)

**Not required for install/releases anymore:** `RELEASE_AWS_ACCESS_KEY_ID` / `RELEASE_AWS_SECRET_ACCESS_KEY` for those upload jobs (still used for sccache / e2e AWS until rust-cache cutover).

Create R2 API tokens in Cloudflare dashboard → R2 → Manage R2 API Tokens → permission **Object Read & Write** on `skippr-web-install` (or account-wide for CI).
