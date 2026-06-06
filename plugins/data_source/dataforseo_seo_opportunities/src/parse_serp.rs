use serde_json::{json, Value};

use crate::config::ScoringConfig;
use crate::target::domain_from_url;

pub struct SerpParseContext<'a> {
    pub site: &'a str,
    pub run_date: &'a str,
    pub keyword: &'a str,
    pub own_domain: &'a str,
    pub competitor_domains: &'a [String],
    pub location_code: u32,
    pub language_code: &'a str,
    pub device: &'a str,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedSerp {
    pub results: Vec<Value>,
    pub features: Vec<Value>,
}

pub fn parse_serp_items(items: &[Value], ctx: &SerpParseContext<'_>) -> ParsedSerp {
    let mut parsed = ParsedSerp::default();
    let keyword_lower = ctx.keyword.to_lowercase();
    for item in items {
        let item_type = item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        match item_type {
            "organic" => {
                if let Some(row) = parse_organic_item(item, ctx, &keyword_lower) {
                    parsed.results.push(row);
                }
            }
            "featured_snippet" | "people_also_ask" | "local_pack" | "video" | "images"
            | "shopping" | "knowledge_graph" | "answer_box" | "ai_overview" => {
                parsed
                    .features
                    .extend(parse_feature_item(item, ctx, item_type));
            }
            _ => {
                if item.get("url").is_some() {
                    if let Some(row) = parse_organic_item(item, ctx, &keyword_lower) {
                        parsed.results.push(row);
                    }
                } else if item.get("items").is_some() {
                    parsed
                        .features
                        .extend(parse_feature_item(item, ctx, item_type));
                }
            }
        }
    }
    parsed
}

fn parse_organic_item(
    item: &Value,
    ctx: &SerpParseContext<'_>,
    keyword_lower: &str,
) -> Option<Value> {
    let url = item.get("url").and_then(|v| v.as_str())?;
    let domain = item
        .get("domain")
        .and_then(|v| v.as_str())
        .map(str::to_lowercase)
        .or_else(|| domain_from_url(url))
        .unwrap_or_default();
    let title = item
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let description = item
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let rank_absolute = item.get("rank_absolute").and_then(json_u32).unwrap_or(0);
    let rank_group = item.get("rank_group").and_then(json_u32).unwrap_or(0);
    let estimated_domain_rank = item
        .get("rank_info")
        .and_then(|v| v.get("main_domain_rank"))
        .and_then(json_u32)
        .or_else(|| {
            item.get("rank_info")
                .and_then(|v| v.get("page_rank"))
                .and_then(json_u32)
        });
    let title_match_score = title_match(keyword_lower, title);
    let snippet_match_score = snippet_match(keyword_lower, description);
    let content_type = classify_content_type(&domain, url, title);
    Some(json!({
        "site": ctx.site,
        "run_date": ctx.run_date,
        "keyword": ctx.keyword,
        "rank_absolute": rank_absolute,
        "rank_group": rank_group,
        "result_type": "organic",
        "url": url,
        "domain": domain,
        "title": title,
        "description": description,
        "breadcrumb": item.get("breadcrumb").cloned().unwrap_or(Value::Null),
        "is_own_domain": domain == ctx.own_domain,
        "is_competitor_domain": ctx.competitor_domains.iter().any(|d| d == &domain),
        "estimated_domain_rank": estimated_domain_rank,
        "page_backlink_count": null,
        "referring_domains": null,
        "title_match_score": title_match_score,
        "snippet_match_score": snippet_match_score,
        "content_type": content_type,
        "location_code": ctx.location_code,
        "language_code": ctx.language_code,
        "device": ctx.device,
    }))
}

fn parse_feature_item(item: &Value, ctx: &SerpParseContext<'_>, feature_type: &str) -> Vec<Value> {
    let mut rows = Vec::new();
    let position = item.get("rank_absolute").and_then(json_u32);
    if feature_type == "people_also_ask" {
        if let Some(sub_items) = item.get("items").and_then(|v| v.as_array()) {
            for sub in sub_items {
                let question = sub
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let mut answer_text = None;
                let mut source_url = None;
                let mut owned_by_domain = None;
                if let Some(expanded) = sub
                    .get("expanded_element")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first())
                {
                    answer_text = expanded
                        .get("description")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    source_url = expanded
                        .get("url")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    owned_by_domain = source_url.as_deref().and_then(domain_from_url);
                }
                rows.push(json!({
                    "site": ctx.site,
                    "run_date": ctx.run_date,
                    "keyword": ctx.keyword,
                    "feature_type": "people_also_ask",
                    "position": position,
                    "owned_by_domain": owned_by_domain,
                    "source_url": source_url,
                    "question_text": question,
                    "answer_text": answer_text,
                    "feature_occupancy": 1,
                    "capture_possible": owned_by_domain.as_deref() != Some(ctx.own_domain),
                    "location_code": ctx.location_code,
                    "language_code": ctx.language_code,
                    "device": ctx.device,
                }));
            }
        }
        return rows;
    }

    let domain = item
        .get("domain")
        .and_then(|v| v.as_str())
        .map(str::to_lowercase)
        .or_else(|| {
            item.get("url")
                .and_then(|v| v.as_str())
                .and_then(domain_from_url)
        })
        .unwrap_or_default();
    rows.push(json!({
        "site": ctx.site,
        "run_date": ctx.run_date,
        "keyword": ctx.keyword,
        "feature_type": feature_type,
        "position": position,
        "owned_by_domain": if domain.is_empty() { Value::Null } else { json!(domain) },
        "source_url": item.get("url").cloned().unwrap_or(Value::Null),
        "question_text": item.get("title").cloned().unwrap_or(Value::Null),
        "answer_text": item.get("description").cloned().unwrap_or(Value::Null),
        "feature_occupancy": 1,
        "capture_possible": domain != ctx.own_domain,
        "location_code": ctx.location_code,
        "language_code": ctx.language_code,
        "device": ctx.device,
    }));
    rows
}

fn title_match(keyword_lower: &str, title: &str) -> f64 {
    let title_lower = title.to_lowercase();
    if title_lower.contains(keyword_lower) {
        return 1.0;
    }
    let tokens: Vec<&str> = keyword_lower.split_whitespace().collect();
    if tokens.is_empty() {
        return 0.0;
    }
    let matched = tokens.iter().filter(|t| title_lower.contains(**t)).count();
    matched as f64 / tokens.len() as f64
}

fn snippet_match(keyword_lower: &str, snippet: &str) -> f64 {
    let snippet_lower = snippet.to_lowercase();
    if snippet_lower.contains(keyword_lower) {
        return 1.0;
    }
    0.0
}

fn classify_content_type(domain: &str, url: &str, title: &str) -> &'static str {
    let haystack = format!("{domain} {url} {title}").to_lowercase();
    if haystack.contains("reddit.com") || haystack.contains("/r/") {
        return "forum";
    }
    if haystack.contains("quora.com")
        || haystack.contains("stackoverflow.com")
        || haystack.contains("forum.")
    {
        return "ugc";
    }
    if haystack.contains("/blog") || haystack.contains("blog.") {
        return "blog";
    }
    if haystack.contains("wikipedia.org") {
        return "reference";
    }
    "webpage"
}

pub fn is_weak_domain(domain: &str, rank: Option<u32>, threshold: u32) -> bool {
    if domain.contains("reddit.com")
        || domain.contains("quora.com")
        || domain.contains("forum.")
        || domain.contains("stackoverflow.com")
    {
        return true;
    }
    rank.map(|r| r <= threshold).unwrap_or(false)
}

pub fn compute_weak_spots(
    keyword: &str,
    serp_results: &[Value],
    site: &str,
    run_date: &str,
    scoring: &ScoringConfig,
    location_code: u32,
    language_code: &str,
    device: &str,
) -> Vec<Value> {
    let threshold = scoring.weak_domain_rank_threshold;
    let top10: Vec<&Value> = serp_results
        .iter()
        .filter(|r| r.get("rank_absolute").and_then(json_u32).unwrap_or(99) <= 10)
        .collect();
    let top20: Vec<&Value> = serp_results
        .iter()
        .filter(|r| r.get("rank_absolute").and_then(json_u32).unwrap_or(99) <= 20)
        .collect();

    let mut forum_count = 0u32;
    let mut ugc_count = 0u32;
    let mut low_authority_count = 0u32;
    let mut thin_title_count = 0u32;
    let mut missing_exact_title_count = 0u32;
    let mut weak_domains = Vec::new();
    let mut best_weak_rank: Option<u32> = None;

    for row in &top10 {
        let domain = row
            .get("domain")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let content_type = row
            .get("content_type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let rank = row.get("rank_absolute").and_then(json_u32);
        let title_match_score = row
            .get("title_match_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let estimated_rank = row.get("estimated_domain_rank").and_then(json_u32);

        if content_type == "forum" {
            forum_count += 1;
        }
        if content_type == "ugc" {
            ugc_count += 1;
        }
        if is_weak_domain(domain, estimated_rank, threshold) {
            low_authority_count += 1;
            if !weak_domains.iter().any(|d: &String| d == domain) {
                weak_domains.push(domain.to_string());
            }
            if let Some(r) = rank {
                best_weak_rank = Some(best_weak_rank.map(|b| b.min(r)).unwrap_or(r));
            }
        }
        if title_match_score < 0.5 {
            thin_title_count += 1;
        }
        if title_match_score < 0.25 {
            missing_exact_title_count += 1;
        }
    }

    let weakness_score = ((forum_count + ugc_count) as f64 * 12.0
        + low_authority_count as f64 * 15.0
        + thin_title_count as f64 * 5.0
        + missing_exact_title_count as f64 * 8.0)
        .min(100.0);

    vec![json!({
        "site": site,
        "run_date": run_date,
        "keyword": keyword,
        "weakness_type": "aggregate",
        "weakness_count_top10": top10.len() as u32,
        "weakness_count_top20": top20.len() as u32,
        "best_weak_rank": best_weak_rank,
        "weak_domains": weak_domains,
        "forum_count": forum_count,
        "ugc_count": ugc_count,
        "low_authority_count": low_authority_count,
        "thin_title_count": thin_title_count,
        "missing_exact_title_count": missing_exact_title_count,
        "weakness_score": weakness_score,
        "location_code": location_code,
        "language_code": language_code,
        "device": device,
    })]
}

fn json_u32(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| value.as_i64().and_then(|n| u32::try_from(n).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::parse_live_response;
    use crate::config::ScoringConfig;

    #[test]
    fn parse_serp_from_fixture() {
        let bytes = std::fs::read(format!(
            "{}/fixtures/serp_organic_live.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let parsed = parse_live_response(&body).unwrap();
        let ctx = SerpParseContext {
            site: "picnic.com",
            run_date: "2026-05-30",
            keyword: "meal planning app free",
            own_domain: "picnic.com",
            competitor_domains: &[],
            location_code: 2840,
            language_code: "en",
            device: "desktop",
        };
        let serp = parse_serp_items(&parsed.tasks[0].items, &ctx);
        assert!(!serp.results.is_empty());
        assert!(!serp.features.is_empty());
        let weak = compute_weak_spots(
            "meal planning app free",
            &serp.results,
            "picnic.com",
            "2026-05-30",
            &ScoringConfig::default(),
            2840,
            "en",
            "desktop",
        );
        assert_eq!(weak.len(), 1);
        assert!(weak[0]["weakness_score"].as_f64().unwrap_or(0.0) > 0.0);
    }
}
