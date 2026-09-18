# Google PageSpeed Insights plugin

Runtime source plugin: `plugins/data_source/google_pagespeed/` (`GooglePageSpeed` / `google_pagespeed`).

## Overview

Calls the [PageSpeed Insights API v5](https://developers.google.com/speed/docs/insights/v5/get-started) (`runPagespeed`) for a daily sample of URLs (mobile + desktop). Emits bronze namespaces with `replace_partition` on `run_date` and `semantics: mutable_report`.

## Namespaces

| Namespace | Grain |
| --- | --- |
| `google_pagespeed.site_run_daily` | Site rollup per run |
| `google_pagespeed.page_daily` | URL × strategy |
| `google_pagespeed.field_origin_daily` | Origin CrUX (when API returns `originLoadingExperience`) |
| `google_pagespeed.audit_daily` | Top failing Lighthouse audits |
| `google_pagespeed.check_daily` | Threshold check outcomes (e.g. `FIELD_LCP_SLOW`) |

## Configuration (`skippr.yml`)

```yaml
data_sources:
  example_psi:
    GooglePageSpeed:
      site: "https://example.com"
      api_key: null
      url_mode: tld_sample
      max_urls: 50
      strategies: [mobile, desktop]
      max_requests_per_run: 120
      requests_per_minute: 30
```

Env: `PAGESPEED_API_KEY` (overridden by `api_key` in config). Offline tests: `SKIPPR_GOOGLE_PAGESPEED_FIXTURE_DIR`.

## URL sampling

- `tld_sample`: homepage + robots/sitemap discovery (capped by `max_urls`).
- `url_list`: explicit URL list.

Discover mode: one URL, `mobile` only, `performance` category; no checkpoint writes.

## Checkpoint

Key: `google_pagespeed:run:{run_date}:{site}`. Value: completed `(canonical_url, strategy)` pairs for crash-safe resume within a run date.

## CLI

```bash
skipprd connect data-source google-page-speed \
  --pipeline pagespeed \
  --name pagespeed \
  --site https://example.com \
  --api-key '${PAGESPEED_API_KEY}'
```

## Tests

```bash
export SKIPPR_GOOGLE_PAGESPEED_FIXTURE_DIR=plugins/data_source/google_pagespeed/fixtures
cargo test -p skippr-plugin-data-source-google-pagespeed
```
