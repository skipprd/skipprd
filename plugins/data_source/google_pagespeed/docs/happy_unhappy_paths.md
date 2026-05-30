# Google PageSpeed — happy / unhappy paths

## Happy paths

- Valid `site`, API key or `PAGESPEED_API_KEY` / fixture dir
- URL sampling (`tld_sample` / `url_list`), robots respect on full sync
- Mobile + desktop strategies (discover: mobile only, 1 URL, 1 request)
- Fixture `run_pagespeed_*.json` → `page_daily`, `field_origin_daily`, audits, issues
- Job checkpoint: resume skips completed URL×strategy pairs
- Rate limiting via `requests_per_minute`
- API errors classified → error page row + issue codes (`INVALID_URL`, quota, etc.)
- Discover: no checkpoint load/store; performance category only
- Five namespaces, `ReplacePartition` on `run_date`

## Unhappy paths

- Missing API key without fixture env → init error
- Invalid / empty site → sampling error
- PageSpeed API failure → synthetic error row, issues, still advances checkpoint on sync
- No CrUX field data → `NO_FIELD_DATA` issue
- Slow LCP category → `FIELD_LCP_SLOW` issue
- HTTP 429 → retry (client); 400 → give up
- Discover must not call `store_checkpoint`
