# DataForSEO Backlinks plugin

Runtime source plugin `DataForSeoBacklinks` (`plugins/data_source/dataforseo_backlinks/`).

## API

- [backlinks/live](https://docs.dataforseo.com/v3/backlinks-backlinks/live/)
- [summary/live](https://docs.dataforseo.com/v3/backlinks-summary-live/)
- [referring_domains/live](https://docs.dataforseo.com/v3/backlinks-referring_domains-live/)
- [anchors/live](https://docs.dataforseo.com/v3/backlinks-anchors-live/)
- [history/live](https://docs.dataforseo.com/v3/backlinks-history-live/)
- [page_intersection/live](https://docs.dataforseo.com/v3/backlinks-page_intersection-live/)

HTTP Basic auth (`DATAFORSEO_API_USER`, `DATAFORSEO_API_PASS`, or legacy `DATAFORSEO_LOGIN` / `DATAFORSEO_PASSWORD`).

## Namespaces

| Namespace | Grain | `write_policy` |
| --- | --- | --- |
| `dataforseo_backlinks.site_run_daily` | 1 / run | `replace_partition` on `run_date` |
| `dataforseo_backlinks.backlink_daily` | 1 / link / entity / day | `replace_partition` on `run_date` |
| `dataforseo_backlinks.summary_daily` | 1 / entity / day | `replace_partition` on `run_date` |
| `dataforseo_backlinks.referring_domain_daily` | 1 / referring domain / entity / day | `replace_partition` on `run_date` |
| `dataforseo_backlinks.anchor_daily` | 1 / anchor / entity / day | `replace_partition` on `run_date` |
| `dataforseo_backlinks.history_daily` | 1 / monthly history point / entity / day | `replace_partition` on `run_date` |
| `dataforseo_backlinks.page_intersection_daily` | 1 / referring page / job / day | `replace_partition` on `run_date` |

All row-level namespaces include `entity_kind` (`primary` | `competitor`), `competitor_name` (null for primary), `target`, and `site`.

## Configuration

```yaml
data_sources:
  example:
    DataForSeoBacklinks:
      site: example.com
      backlink_jobs:
        - target: example.com
          job_tag: main
          limit: 100
          max_pages: 5
      competitors:
        - name: "Rival"
          target: rival.com
      streams:
        - backlinks
        - summary
        - referring_domains
        - anchors
        - history
        - page_intersection
      history:
        date_from: "2024-01-01"
```

- `streams`: optional; defaults to all streams when omitted.
- `competitors`: optional; each entry is synced with the same stream set as primary (backlink jobs cloned from primary templates).
- `page_intersection`: primary config only unless competitor domains appear in job `targets`.

## Discover

`skipprd discover` uses `limit: 5`, `max_pages: 1`, the first primary entity only, and does not advance pagination checkpoints.

## Fixtures

Set `SKIPPR_DATAFORSEO_BACKLINKS_FIXTURE_DIR` to the crate `fixtures/` directory for offline tests.

## CLI

```bash
sde connect source dataforseo-backlinks \
  --login "${DATAFORSEO_API_USER}" \
  --password "${DATAFORSEO_API_PASS}" \
  --site example.com \
  --backlink-target example.com
```

For `competitors`, `streams`, `intersection_jobs`, and multiple `backlink_jobs`, edit engine `skippr.yml` under `DataForSeoBacklinks:`.
