# Site Quality — happy / unhappy paths

## Happy paths

- Valid `site`, devices, `url_mode` (`tld_sample` or `url_list`)
- URL sampling: robots/sitemap discovery or explicit list, capped by `max_pages_per_run`
- Node worker fixture via `SKIPPR_SITE_QUALITY_FIXTURE_DIR` (no live Playwright in CI)
- Per URL × device: lab metrics → `page_lab_daily`, issues, optional Lighthouse/axe namespaces
- `render_hash` checkpoint: skip heavy Lighthouse/axe when unchanged (`skip_heavy_when_unchanged`)
- Discover: homepage + mobile only, no checkpoint store
- Normal sync: checkpoint store per URL/device after successful job
- Five namespace contracts (`ReplacePartition`, `run_date` partition)

## Unhappy paths

- Empty `site`, no devices, `url_list` mode without URLs, `max_pages_per_run == 0` → validation error
- Worker job failure → `NAVIGATION_TIMEOUT` / error issue rows, `pages_failed` increment
- HTTP ≥ 400 → `HTTP_ERROR` issue
- CLS/LCP/Lighthouse thresholds → derived issue codes
- Robots disallow filters paths in TLD sampling
- Discover must not persist checkpoints
- Missing worker fixture file → job error path (when not using fixture env)
