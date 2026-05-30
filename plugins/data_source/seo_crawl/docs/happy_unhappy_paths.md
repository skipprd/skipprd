# SeoCrawl — happy / unhappy paths

## Happy paths

- Valid `site` URL normalized to `https://{host}` origin
- `robots.txt` fetch + parse (`Disallow`, `Sitemap` directives)
- Sitemap XML `urlset` / `sitemapindex` → seed URLs
- Bounded BFS crawl (`max_urls`, `max_depth`) same-origin links only
- HTML parse: title, meta, canonical, links, `content_hash`, issues
- Content blocks extracted (H1, main text) → `seo_crawl.content_block`
- OpenAI per-block JSON via `OpenAiChatClient` + `SKIPPR_OPENAI_FIXTURE_DIR` in CI
- Page checkpoint: `content_hash` + block hashes; skip OpenAI when unchanged
- Fixture HTTP via `SKIPPR_SEO_CRAWL_FIXTURE_DIR` (pages/, robots.txt, sitemap.xml)
- Discover: cap URLs/depth, no checkpoint I/O
- Normal sync: page rows + block rows + checkpoint store
- Seven namespace contracts (`ReplacePartition`, `crawl_date`)

## Unhappy paths

- Empty `site` or `max_urls == 0` → config validation error
- Invalid site URL → normalize error
- Malformed sitemap XML → parse error (propagates from `parse_sitemap_xml`)
- Disallowed path (`/private`) skipped when `respect_robots`
- HTTP 4xx page skipped (no row)
- Missing `OPENAI_API_KEY` when OpenAI enabled without fixture → client absent; blocks without analysis
- Non-Instagram / off-site links filtered via `normalize_url_for_crawl`
- Discover must not `store_checkpoint`
- Empty crawl result still completes sync (no page rows)
