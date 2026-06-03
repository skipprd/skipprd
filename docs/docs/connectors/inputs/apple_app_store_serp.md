# Apple App Store SERP

Track App Store search positions for your iOS/macOS apps on a small set of keywords and storefronts. Uses the public [iTunes Search API](https://developer.apple.com/library/archive/documentation/AudioVideo/Conceptual/iTuneSearchAPI/) — no API key required.

## Connect

```bash
skippr connect source apple-app-store-serp \
  --app-id 123456789 \
  --bundle-id com.example.app \
  --keywords "photo editor,image editor" \
  --storefronts us,gb
```

## Options (engine `skippr.yml`)

| Field | Description |
| --- | --- |
| `targets` | Apps to find (`app_id`, optional `bundle_id`, optional `aliases` of competitor trackIds) |
| `keywords` | Search terms to run |
| `storefronts` | ISO country codes (e.g. `us`, `gb`) — at least one required |
| `entity` | `software`, `iPadSoftware`, or `macSoftware` (default `software`) |
| `max_depth` | Max results to scan (default 50, max 200) |
| `min_query_interval_ms` | Pause between keyword×storefront pairs (default 3000) |
| `max_queries_per_run` | Cap total pairs per sync (keywords × storefronts) |
| `capture_results` | Store full result rows (default false) |

## Operational notes

- Rank is the 1-based index in the iTunes API `results[]` list; ordering may differ slightly from the App Store app UI.
- Keep `min_query_interval_ms` at 3000 or higher when scanning many storefronts.
- Same keyword/storefront/entity is skipped on subsequent syncs the same UTC day unless `force_refresh_today: true`.

See [maintainer doc](../../maintainers/apple-app-store-serp-plugin.md) for namespaces and testing.
