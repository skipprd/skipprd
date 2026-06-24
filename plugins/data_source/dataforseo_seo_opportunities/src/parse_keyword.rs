use serde_json::{json, Value};

use crate::config::SeedSource;

pub struct KeywordParseContext<'a> {
    pub site: &'a str,
    pub run_date: &'a str,
    pub seed_keyword: &'a str,
    pub seed_source: SeedSource,
    pub location_code: u32,
    pub language_code: &'a str,
    pub device: &'a str,
    pub suggestion_source: &'a str,
}

pub fn parse_seed_rows(ctx: &KeywordParseContext<'_>, priority: u32) -> Vec<Value> {
    vec![json!({
        "site": ctx.site,
        "run_date": ctx.run_date,
        "seed_keyword": ctx.seed_keyword,
        "seed_source": seed_source_str(ctx.seed_source),
        "source_page": null,
        "gsc_clicks": null,
        "gsc_impressions": null,
        "source_url": null,
        "priority": priority,
        "location_code": ctx.location_code,
        "language_code": ctx.language_code,
        "device": ctx.device,
    })]
}

pub fn parse_keyword_suggestion_items(
    items: &[Value],
    ctx: &KeywordParseContext<'_>,
) -> (Vec<Value>, Vec<Value>) {
    let mut suggestions = Vec::new();
    let mut metrics = Vec::new();
    for item in items {
        let keyword = item
            .get("keyword")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if keyword.is_empty() {
            continue;
        }
        let info = item.get("keyword_info").cloned().unwrap_or(Value::Null);
        let serp_info = item.get("serp_info").cloned().unwrap_or(Value::Null);
        let search_volume = info.get("search_volume").and_then(json_u64).unwrap_or(0);
        let cpc = info.get("cpc").and_then(json_f64);
        let competition = info.get("competition").and_then(json_f64);
        let keyword_properties = item
            .get("keyword_properties")
            .cloned()
            .unwrap_or(Value::Null);
        let keyword_difficulty = keyword_properties
            .get("keyword_difficulty")
            .or_else(|| info.get("keyword_difficulty"))
            .and_then(json_u64)
            .map(|n| n as u32);
        let intent = item
            .get("search_intent_info")
            .or_else(|| info.get("search_intent_info"))
            .and_then(|v| v.get("main_intent"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let serp_item_types = serp_info
            .get("serp_item_types")
            .cloned()
            .unwrap_or(json!([]));
        let word_count = keyword.split_whitespace().count() as u32;
        let is_question = keyword.contains('?')
            || keyword.starts_with("how ")
            || keyword.starts_with("what ")
            || keyword.starts_with("why ")
            || keyword.starts_with("when ")
            || keyword.starts_with("where ")
            || keyword.starts_with("who ")
            || keyword.starts_with("can ")
            || keyword.starts_with("is ")
            || keyword.starts_with("are ");
        suggestions.push(json!({
            "site": ctx.site,
            "run_date": ctx.run_date,
            "keyword": keyword,
            "seed_keyword": ctx.seed_keyword,
            "suggestion_source": ctx.suggestion_source,
            "search_volume": search_volume,
            "cpc": cpc,
            "competition": competition,
            "keyword_difficulty": keyword_difficulty,
            "intent": intent,
            "monthly_trend": info.get("monthly_searches").cloned().unwrap_or(Value::Null),
            "word_count": word_count,
            "is_question": is_question,
            "contains_modifier": word_count > 2,
            "serp_item_types": serp_item_types,
            "location_code": ctx.location_code,
            "language_code": ctx.language_code,
            "device": ctx.device,
        }));
        metrics.push(json!({
            "site": ctx.site,
            "run_date": ctx.run_date,
            "keyword": keyword,
            "search_volume": search_volume,
            "cpc": cpc,
            "competition": competition,
            "keyword_difficulty": keyword_difficulty,
            "intent": intent,
            "monthly_searches": info.get("monthly_searches").cloned().unwrap_or(Value::Null),
            "location_code": ctx.location_code,
            "language_code": ctx.language_code,
            "device": ctx.device,
        }));
    }
    (suggestions, metrics)
}

pub fn parse_search_volume_items(
    items: &[Value],
    site: &str,
    run_date: &str,
    location_code: u32,
    language_code: &str,
    device: &str,
) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| {
            let keyword = item.get("keyword")?.as_str()?.trim();
            if keyword.is_empty() {
                return None;
            }
            Some(json!({
                "site": site,
                "run_date": run_date,
                "keyword": keyword,
                "search_volume": item.get("search_volume").and_then(json_u64).unwrap_or(0),
                "cpc": item.get("cpc").and_then(json_f64),
                "competition": item.get("competition").and_then(json_f64),
                "keyword_difficulty": null,
                "intent": null,
                "monthly_searches": item.get("monthly_searches").cloned().unwrap_or(Value::Null),
                "location_code": location_code,
                "language_code": language_code,
                "device": device,
            }))
        })
        .collect()
}

fn seed_source_str(source: SeedSource) -> &'static str {
    match source {
        SeedSource::Config => "config",
        SeedSource::Gsc => "gsc",
        SeedSource::Crawl => "crawl",
        SeedSource::Competitor => "competitor",
        SeedSource::Autocomplete => "autocomplete",
    }
}

fn json_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
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
    fn parse_suggestions_from_fixture() {
        let bytes = std::fs::read(format!(
            "{}/fixtures/keyword_suggestions_live.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let parsed = parse_live_response(&body).unwrap();
        let ctx = KeywordParseContext {
            site: "example.com",
            run_date: "2026-05-30",
            seed_keyword: "meal planning app",
            seed_source: SeedSource::Config,
            location_code: 2840,
            language_code: "en",
            device: "desktop",
            suggestion_source: "labs_keyword_suggestions",
        };
        let (suggestions, metrics) = parse_keyword_suggestion_items(&parsed.tasks[0].items, &ctx);
        assert_eq!(suggestions.len(), 3);
        assert_eq!(metrics.len(), 3);
        assert_eq!(suggestions[0]["keyword"], "meal planning app free");
        assert_eq!(suggestions[0]["keyword_difficulty"], 28);
        assert_eq!(suggestions[0]["intent"], "commercial");
        assert_eq!(metrics[2]["keyword_difficulty"], 18);
        assert_eq!(metrics[2]["intent"], "informational");
    }

    #[test]
    fn parse_search_volume_items_from_fixture() {
        let bytes = std::fs::read(format!(
            "{}/fixtures/search_volume_live.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let parsed = parse_live_response(&body).unwrap();
        let rows = parse_search_volume_items(
            &parsed.tasks[0].items,
            "example.com",
            "2026-05-30",
            2840,
            "en",
            "desktop",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["search_volume"], 2400);
    }
}
