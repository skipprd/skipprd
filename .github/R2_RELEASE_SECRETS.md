# GitHub Actions secrets for Cloudflare R2 (install CDN + sccache)

Add these repository secrets on **skipprd/skipprd-private** (Settings → Secrets and variables → Actions).
Also add them to the **Skippr** environment if that environment overrides secret scope for publish/build jobs.

| Secret | Value | Notes |
|--------|-------|-------|
| `R2_ACCOUNT_ID` | Cloudflare account ID | Same as `CLOUDFLARE_ACCOUNT_ID` in `cloud/.env` |
| `R2_ACCESS_KEY_ID` | R2 S3 API access key | Same as `OBJECTS_ACCESS_KEY_ID` |
| `R2_SECRET_ACCESS_KEY` | R2 S3 API secret | Same as `OBJECTS_SECRET_ACCESS_KEY` |

Endpoint used by CI: `https://${R2_ACCOUNT_ID}.r2.cloudflarestorage.com`

| Bucket | Role |
|--------|------|
| `skippr-web-install` | Public install + releases (`install.skippr.io`) |
| `skippr-web-rust-cache` | Private sccache + Windows vcpkg binary cache |

Token permission: **Object Read & Write** on both buckets (or account-wide).

**Still AWS (not R2):** `RELEASE_AWS_ACCESS_KEY_ID` / `RELEASE_AWS_SECRET_ACCESS_KEY` for CodeArtifact only. E2E jobs keep using `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` for real AWS resources.
