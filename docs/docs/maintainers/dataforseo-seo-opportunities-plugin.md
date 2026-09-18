# DataForSEO SEO Opportunities plugin

Runtime source plugin `DataForSeoSeoOpportunities` (`plugins/data_source/dataforseo_seo_opportunities/`).

LowFruits-style keyword opportunity intelligence: keyword expansion, SERP weakness scoring, allintitle/KGR, competitor keyword extraction, rank tracking, AI citation opportunities, and content briefs.

## API

- [keyword_suggestions/live](https://docs.dataforseo.com/v3/dataforseo_labs/google/keyword_suggestions/live/)
- [search_volume/live](https://docs.dataforseo.com/v3/keywords_data/google_ads/search_volume/live/)
- [organic/live/advanced](https://docs.dataforseo.com/v3/serp/google/organic/live/advanced/)
- [ranked_keywords/live](https://docs.dataforseo.com/v3/dataforseo_labs/google/ranked_keywords/live/)
- [related_keywords/live](https://docs.dataforseo.com/v3/dataforseo_labs/google/related_keywords/live/)
- [autocomplete/live/advanced](https://docs.dataforseo.com/v3/serp/google/autocomplete/live/advanced/)

HTTP Basic auth (`DATAFORSEO_API_USER`, `DATAFORSEO_API_PASS`, or legacy `DATAFORSEO_LOGIN` / `DATAFORSEO_PASSWORD`).

## Namespaces

| Namespace | Grain | `write_policy` |
| --- | --- | --- |
| `dataforseo_seo_opportunities.site_run_daily` | 1 / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.seed_keyword_daily` | 1 / seed / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.keyword_suggestion_daily` | 1 / keyword / seed / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.keyword_metric_daily` | 1 / keyword / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.serp_result_daily` | 1 / keyword / rank / URL / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.serp_feature_daily` | 1 / keyword / feature / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.weak_spot_daily` | 1 / keyword / weakness / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.opportunity_score_daily` | 1 / keyword / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.keyword_cluster_daily` | 1 / cluster / keyword / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.competitor_keyword_daily` | 1 / competitor / keyword / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.competitor_sitemap_url_daily` | 1 / competitor / URL / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.allintitle_daily` | 1 / keyword / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.rank_tracking_daily` | 1 / keyword / domain / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.ai_citation_opportunity_daily` | 1 / query / run | `replace_partition` on `run_date` |
| `dataforseo_seo_opportunities.content_brief_daily` | 1 / brief / run | `replace_partition` on `run_date` |

## Configuration

```yaml
data_sources:
  seo_opportunities:
    DataForSeoSeoOpportunities:
      site: example.com
      location_code: 2840
      language_code: en
      device: desktop
      run_mode: mvp
      seed_keywords:
        - "meal planning app"
      streams:
        - keyword_suggestions
        - keyword_metrics
        - serp_results
        - serp_features
        - weak_spots
        - opportunity_scores
      limits:
        serp_depth: 20
      scoring:
        weak_domain_rank_threshold: 40
        include_allintitle: true
        include_kgr: true
```

- `run_mode`: `mvp` (default streams), `full` (all streams), or `discover_only`.
- `streams`: optional; defaults depend on `run_mode`.
- `competitors`: optional list of `{ name, domain }` for ranked keyword and sitemap extraction.
- `rank_track_keywords`: optional explicit keywords for rank tracking (defaults to analyzed keywords).

## Discover

`skipprd discover` uses one seed keyword, `limit: 5` suggestions, SERP depth 10, primary location only, and no competitor sitemap fetches.

## Fixtures

Set `SKIPPR_DATAFORSEO_SEO_OPPORTUNITIES_FIXTURE_DIR` to the crate `fixtures/` directory for offline tests.

## CLI

```bash
skipprd connect data-source data-for-seo-seo-opportunities \
  --pipeline seo \
  --name opportunities \
  --login '${DATAFORSEO_API_USER}' \
  --password '${DATAFORSEO_API_PASS}' \
  --site example.com
```

List fields (`seed_keywords`, `competitors`, `streams`) stay in engine `skippr.yml` under `DataForSeoSeoOpportunities:`. Flattened `--limits-*` and `--scoring-*` flags match the nested YAML keys.

## API cost drivers

- Number of seed keywords and generated suggestions
- SERP depth per keyword (`limits.serp_depth`)
- Allintitle queries (`scoring.include_allintitle`)
- Competitor ranked keyword and sitemap extraction
- Location/device permutations

## Tests

```bash
cargo test -p skippr-plugin-data-source-dataforseo-seo-opportunities
```
