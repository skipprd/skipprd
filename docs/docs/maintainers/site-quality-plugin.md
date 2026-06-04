# Site Quality source plugin

Runtime source plugin: `plugins/data_source/site_quality/` (`SiteQuality` / `site_quality`).

Playwright lab sessions per URL × device profile (mobile + desktop), with social preview metadata extraction plus optional axe-core and Lighthouse (CDP). Bronze namespaces use `replace_partition` on `run_date`.

## Bronze catalog

| Namespace | Grain |
| --- | --- |
| `site_quality.site_run_daily` | 1 / run |
| `site_quality.page_lab_daily` | URL × device / day |
| `site_quality.a11y_issue` | axe violation / day |
| `site_quality.check_daily` | Per-check outcomes (pass / warn / fail) / day |
| `site_quality.lighthouse_audit` | failing audit / day |

## Discover sampling (automatic)

When `SKIPPR_RUNTIME_EXECUTION_MODE=discover`:

- Homepage only, **mobile** device profile
- No checkpoint load/advance
- Namespace contracts still reflect full configured options (lighthouse/axe flags)

## Configuration (engine `skippr.yml`)

```yaml
data_sources:
  example_site:
    SiteQuality:
      site: "https://example.com"
      url_mode: tld_sample
      max_pages_per_run: 50
      lighthouse_enabled: true
      axe_enabled: true
      pages_per_minute: 6
```

`url_mode: url_list` requires `url_list: [...]`.

### Device profiles (mobile-first lab)

Default `devices` (when omitted):

| `profile` | Viewport | Lighthouse `form_factor` |
| --- | --- | --- |
| `mobile` | 390×844 | `mobile` |
| `desktop` | 1350×940 | `desktop` |

Override in engine `skippr.yml` or public `skippr.yaml` (`devices` on `site_quality` source):

```yaml
devices:
  - profile: mobile
    viewport: { width: 390, height: 844 }
  - profile: desktop
    viewport: { width: 1350, height: 940 }
```

Full sync runs **one lab job per URL per device** (`max_pages_per_run × len(devices)` jobs). Mobile-only scorecard checks (`MISSING_VIEWPORT`, `HORIZONTAL_SCROLL`, `TEXT_TOO_SMALL`, `TAP_TARGETS`) emit only when `device_profile == mobile`. Web Vitals scorecard checks (`CLS_POOR`, `LCP_SLOW`, `TTFB_SLOW`, `INP_SLOW`) emit per device. `SOCIAL_PREVIEW_METADATA` emits per device from rendered social preview tags.

## Worker sidecar

Bundled Node worker: `plugins/data_source/site_quality/worker/site-quality-worker.mjs`

- JSON-lines job on stdin, one result line on stdout
- Job fields include `device_profile`, `viewport`, `user_agent`, `lighthouse_form_factor` (`mobile` \| `desktop`), `web_vitals_settle_ms`, `collect_inp`
- Result fields include `social_preview`, with normalized title, description, image, URL, generic card fields, and missing-field lists used by `SOCIAL_PREVIEW_METADATA`.
- Dependencies: `playwright-core`, `@axe-core/playwright`, `lighthouse`, `chrome-launcher`, `web-vitals` (production worker override)

Install once per machine:

```bash
cd plugins/data_source/site_quality/worker && npm install
npx playwright install chromium
```

The worker script is resolved automatically (plugin crate `worker/`, skipprd repo layout from cwd or executable, or local runtime manifest dir). Optional override: `SKIPPR_SITE_QUALITY_WORKER_SCRIPT`.

## Local verification

```bash
cargo build -p skippr-plugin-data-source-site-quality
cargo test -p skippr-plugin-data-source-site-quality
cargo test -p skippr-cli translate_site_quality_source
```

Fixture-only sync (no live browser):

```bash
export SKIPPR_SITE_QUALITY_FIXTURE_DIR=plugins/data_source/site_quality/fixtures
export USE_LOCAL_PLUGIN_CODE=1
manifest_dir="$(python3 .github/scripts/local_runtime_plugins.py \
  --config path/to/skippr.yml --pipeline my_pipeline)"
export SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR="$manifest_dir"
skippr discover --pipeline my_pipeline
skippr sync --once --pipeline my_pipeline
```

## CLI

```bash
skippr connect source site-quality \
  --site https://example.com \
  --url-mode tld_sample \
  --max-pages-per-run 50
```

## Checkpointing

Per URL + device key: `site_quality:page:{canonical_url}:{device_profile}`.

When `render_hash` matches the prior run, the worker skips Lighthouse and axe (`skip_heavy_when_unchanged`, default `true`) but still emits `page_lab_daily` for partition completeness.

Public connector: [skippr-web Site Quality](https://github.com/skippr-io/skippr-web/blob/main/docs/connectors/sources/site-quality.md).
