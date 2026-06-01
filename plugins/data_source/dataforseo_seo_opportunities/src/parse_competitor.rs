use serde_json::{json, Value};

pub struct CompetitorKeywordContext<'a> {
    pub site: &'a str,
    pub run_date: &'a str,
    pub competitor_name: &'a str,
    pub domain: &'a str,
    pub own_domain: &'a str,
    pub location_code: u32,
    pub language_code: &'a str,
    pub device: &'a str,
}

pub fn parse_ranked_keyword_items(
    items: &[Value],
    ctx: &CompetitorKeywordContext<'_>,
) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| {
            let keyword_data = item.get("keyword_data")?;
            let keyword = keyword_data.get("keyword")?.as_str()?.trim();
            if keyword.is_empty() {
                return None;
            }
            let info = keyword_data.get("keyword_info").cloned().unwrap_or(Value::Null);
            let serp = item.get("ranked_serp_element")?;
            let serp_item = serp.get("serp_item")?;
            let rank = serp_item
                .get("rank_absolute")
                .or_else(|| serp_item.get("rank_group"))
                .and_then(json_u32);
            let url = serp_item.get("url").and_then(|v| v.as_str());
            let title = serp_item.get("title").and_then(|v| v.as_str());
            let search_volume = info.get("search_volume").and_then(json_u64).unwrap_or(0);
            let difficulty = info
                .get("keyword_difficulty")
                .and_then(json_u32);
            let traffic_estimate = search_volume
                .checked_mul(rank.map(|r| if r <= 10 { 11 - r as u64 } else { 1 }).unwrap_or(1))
                .unwrap_or(0);
            Some(json!({
                "site": ctx.site,
                "run_date": ctx.run_date,
                "competitor_name": ctx.competitor_name,
                "domain": ctx.domain,
                "keyword": keyword,
                "rank": rank,
                "url": url,
                "title": title,
                "search_volume": search_volume,
                "difficulty": difficulty,
                "traffic_estimate": traffic_estimate,
                "is_gap": true,
                "our_best_page": null,
                "our_current_rank": null,
                "location_code": ctx.location_code,
                "language_code": ctx.language_code,
                "device": ctx.device,
            }))
        })
        .collect()
}

pub fn parse_sitemap_urls(
    site: &str,
    run_date: &str,
    competitor_name: &str,
    sitemap_file: &str,
    urls: &[String],
    location_code: u32,
    language_code: &str,
    device: &str,
) -> Vec<Value> {
    urls.iter()
        .map(|url| {
            let path_tokens: Vec<&str> = url
                .split("://")
                .nth(1)
                .unwrap_or(url.as_str())
                .split('/')
                .skip(1)
                .filter(|s| !s.is_empty())
                .collect();
            let slug_keywords = path_tokens.join(" ").replace('-', " ");
            json!({
                "site": site,
                "run_date": run_date,
                "competitor_name": competitor_name,
                "sitemap_file": sitemap_file,
                "url": url,
                "path_tokens": path_tokens,
                "slug_keywords": slug_keywords,
                "lastmod": null,
                "inferred_intent": infer_intent(&slug_keywords),
                "mapped_cluster_id": null,
                "location_code": location_code,
                "language_code": language_code,
                "device": device,
            })
        })
        .collect()
}

fn infer_intent(slug: &str) -> &'static str {
    let lower = slug.to_lowercase();
    if lower.contains("pricing") || lower.contains("buy") {
        "commercial"
    } else if lower.contains("blog") || lower.contains("guide") || lower.contains("how") {
        "informational"
    } else if lower.contains("compare") || lower.contains("vs") {
        "comparison"
    } else {
        "unknown"
    }
}

fn json_u32(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| value.as_i64().and_then(|n| u32::try_from(n).ok()))
}

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::parse_live_response;

    #[test]
    fn parse_ranked_keywords_fixture() {
        let bytes = std::fs::read(format!(
            "{}/fixtures/ranked_keywords_live.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let parsed = parse_live_response(&body).unwrap();
        let ctx = CompetitorKeywordContext {
            site: "picnic.com",
            run_date: "2026-05-30",
            competitor_name: "Rival",
            domain: "rival.com",
            own_domain: "picnic.com",
            location_code: 2840,
            language_code: "en",
            device: "desktop",
        };
        let rows = parse_ranked_keyword_items(&parsed.tasks[0].items, &ctx);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["keyword"], "family calendar app");
    }
}
