# Google SERP ranks source plugin

Runtime source plugin: `plugins/data_source/google_serp_ranks/` (`GoogleSerpRanks` / `google_serp_ranks`).

Low-volume Google organic rank tracking for configured target domains via the **Bright Data SERP API** (`POST https://api.brightdata.com/request`). Set `BRIGHTDATA_API_KEY` in the runtime environment. Optional config: `brightdata_zone` (default `serp_api1`). `fetch_backend` in bronze is `brightdata`.

This is **not** a general SERP harvester: keep query counts small and respect `min_query_interval_ms`. CAPTCHA or consent walls are recorded as `blocked` runs when the API returns them.

## Bronze catalog

| Namespace | Grain |
| --- | --- |
| `google_serp_ranks.run_daily` | 1 / keyword / locale / device / day |
| `google_serp_ranks.target_rank_daily` | 1 / keyword / target / locale / device / day |
| `google_serp_ranks.result_daily` | 1 / organic result position / day (optional) |

All namespaces use `replace_partition` on `run_date`.

## Configuration (engine `skippr.yml`)

```yaml
data_sources:
  example_serp:
    GoogleSerpRanks:
      targets:
        - site: example.com
          aliases: [www.example.com]
      keywords:
        - "best accounting software"
      country: uk
      language: en
      device: desktop
      max_depth: 30
      min_query_interval_ms: 30000
      max_queries_per_run: 10
      stop_after_first_target_match: true
      capture_results: false
      brightdata_zone: serp_api1
```

Hard caps: `max_depth` ≤ 100, `max_queries_per_run` ≤ 100, `min_query_interval_ms` ≥ 5000.

## Bright Data API

The plugin calls Bright Data with `format: raw` and `data_format: parsed_light`, parsing the `organic[]` array (`link`, `title`, `description`, `global_rank`). See [Bright Data SERP API](https://docs.brightdata.com/scraping-automation/serp-api/send-your-first-request).

## Worker sidecar (legacy)

Bundled Node worker: `plugins/data_source/google_serp_ranks/worker/google-serp-worker.mjs`

```bash
cd plugins/data_source/google_serp_ranks/worker && npm install
npx playwright install chromium
```

Script resolution: plugin crate `worker/`, skipprd repo layout, `SKIPPR_LOCAL_RUNTIME_PLUGIN_MANIFEST_DIR`, or `SKIPPR_GOOGLE_SERP_WORKER_SCRIPT`.

## Discover sampling

When `SKIPPR_RUNTIME_EXECUTION_MODE=discover`:

- One keyword, first target only
- `max_depth` capped at 10
- No checkpoint load/advance

## Local verification

```bash
cargo build -p skippr-plugin-data-source-google-serp-ranks
cargo test -p skippr-plugin-data-source-google-serp-ranks
cd plugins/data_source/google_serp_ranks/worker && npm test
```

## Test path matrix

**Happy paths**

| Path | Test |
| --- | --- |
| Valid config / domain dedupe | `config::*`, `domain::*` |
| Sync finds target, emits run + rank (+ optional result) rows | `happy_sync_emits_run_and_target_rows` |
| `capture_results: false` omits `result_daily` | `happy_sync_without_capture_omits_result_namespace` |
| Discover: 1 keyword, no checkpoints | `happy_discover_skips_checkpoints_and_limits_keywords` |
| Same-day checkpoint skips worker | `happy_same_day_checkpoint_skips_worker` |
| `force_refresh_today` re-queries | `happy_force_refresh_runs_despite_checkpoint` |
| `max_queries_per_run` cap | `happy_max_queries_per_run_caps_keywords` |
| SERP URL building / matching (worker lib) | `worker/lib/serp.test.mjs` |

**Unhappy paths**

| Path | Test |
| --- | --- |
| Invalid config (empty targets, keywords, caps) | `config::unhappy_*`, `unhappy_plugin_new_rejects_invalid_config` |
| Blocked page → `blocked` run, `Blocked` checkpoint | `unhappy_blocked_sync_records_blocked_checkpoint` |
| Target not in SERP → `found: false` row | `unhappy_not_found_emits_absent_target_row` |
| Worker navigation error | `unhappy_error_sync_records_error_checkpoint` |
| Bad worker JSON / missing fixture | `worker::parse_*`, `unhappy_fixture_missing_*` |
| CAPTCHA / consent detection | `serp.test.mjs` `detectBlockedPage` |

Fixture-only sync (no live browser):

```bash
export SKIPPR_GOOGLE_SERP_RANKS_FIXTURE_DIR=plugins/data_source/google_serp_ranks/fixtures
```

## CLI

```bash
skipprd connect data-source google-serp-ranks \
  --pipeline google_serp_ranks \
  --name ranks \
  --country uk \
  --language en
```

List fields (`targets`, `keywords`) stay in engine `skippr.yml` under `GoogleSerpRanks:`.
