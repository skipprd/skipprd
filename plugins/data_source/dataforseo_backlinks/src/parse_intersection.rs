use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::config::IntersectionJob;
use crate::parse_backlinks::next_pagination_state;
use crate::parse_backlinks::PaginationAdvance;
use crate::target::normalize_target;

pub use crate::parse_backlinks::next_pagination_state as intersection_next_page;

pub fn build_intersection_task(
    job: &IntersectionJob,
    normalized_targets: &HashMap<String, String>,
    normalized_excludes: &[String],
    limit: u32,
    offset: u32,
    search_after_token: Option<&str>,
    rank_scale: Option<&str>,
) -> Value {
    let mut task = Map::new();
    task.insert("targets".into(), json!(normalized_targets));
    if !normalized_excludes.is_empty() {
        task.insert("exclude_targets".into(), json!(normalized_excludes));
    }
    task.insert("limit".into(), json!(limit));
    if let Some(token) = search_after_token.filter(|t| !t.is_empty()) {
        task.insert("search_after_token".into(), json!(token));
    } else {
        task.insert("offset".into(), json!(offset));
    }
    if let Some(mode) = job
        .intersection_mode
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        task.insert("intersection_mode".into(), json!(mode));
    }
    if let Some(filters) = &job.filters {
        task.insert("filters".into(), filters.clone());
    }
    if let Some(order_by) = &job.order_by {
        task.insert("order_by".into(), json!(order_by));
    }
    if let Some(v) = job.internal_list_limit {
        task.insert("internal_list_limit".into(), json!(v));
    }
    if let Some(scale) = rank_scale.filter(|s| !s.is_empty()) {
        task.insert("rank_scale".into(), json!(scale));
    }
    Value::Object(task)
}

pub fn normalize_intersection_job(
    job: &IntersectionJob,
) -> Result<(HashMap<String, String>, Vec<String>), std::io::Error> {
    let mut targets = HashMap::new();
    for (key, value) in &job.targets {
        targets.insert(key.clone(), normalize_target(value).map_err(std::io::Error::other)?);
    }
    let excludes = job
        .exclude_targets
        .as_ref()
        .map(|list| {
            list.iter()
                .map(|t| normalize_target(t).map_err(std::io::Error::other))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok((targets, excludes))
}

pub fn parse_intersection_items(
    items: &[Value],
    ctx: &IntersectionParseContext,
) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| parse_intersection_item(item, ctx))
        .collect()
}

pub struct IntersectionParseContext<'a> {
    pub run_date: &'a str,
    pub job_name: &'a str,
    pub intersection_mode: Option<&'a str>,
    pub targets_config: &'a Value,
    pub intersections_count: Option<u64>,
    pub api_cost_usd: f64,
}

fn parse_intersection_item(item: &Value, ctx: &IntersectionParseContext<'_>) -> Option<Value> {
    let url_from = item.get("url_from").and_then(|v| v.as_str())?;
    let mut row = Map::new();
    row.insert("job_name".into(), json!(ctx.job_name));
    row.insert("run_date".into(), json!(ctx.run_date));
    row.insert("url_from".into(), json!(url_from));

    for key in [
        "domain_from",
        "page_from_rank",
        "domain_from_rank",
        "anchor",
        "dofollow",
        "rank",
        "backlink_spam_score",
        "is_lost",
    ] {
        if let Some(v) = item.get(key) {
            row.insert(key.to_string(), v.clone());
        }
    }

    if let Some(mode) = ctx.intersection_mode {
        row.insert("intersection_mode".into(), json!(mode));
    }
    row.insert("targets_config".into(), ctx.targets_config.clone());
    row.insert(
        "targets_linked",
        json!(flatten_page_intersection(item.get("page_intersection"))),
    );
    if let Some(count) = ctx.intersections_count {
        row.insert("intersections_count".into(), json!(count));
    }
    row.insert("api_cost_usd".into(), json!(ctx.api_cost_usd));

    Some(Value::Object(row))
}

/// Flatten `page_intersection` target slot maps into `{ "1": true, "2": false, … }`.
pub fn flatten_page_intersection(value: Option<&Value>) -> Map<String, Value> {
    let mut linked = Map::new();
    let Some(obj) = value.and_then(|v| v.as_object()) else {
        return linked;
    };
    for (slot, entry) in obj {
        let present = entry
            .as_object()
            .map(|o| !o.is_empty())
            .unwrap_or_else(|| entry.as_bool().unwrap_or(false));
        linked.insert(slot.clone(), json!(present));
    }
    linked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_intersection_items_flattens_page_intersection() {
        let items = vec![serde_json::json!({
            "url_from": "https://ref.com/a",
            "domain_from": "ref.com",
            "page_intersection": {
                "1": { "rank": 10 },
                "2": {}
            }
        })];
        let targets_config = json!({ "1": "a.com", "2": "b.com" });
        let rows = parse_intersection_items(
            &items,
            &IntersectionParseContext {
                run_date: "2024-06-01",
                job_name: "gap",
                intersection_mode: Some("partial"),
                targets_config: &targets_config,
                intersections_count: Some(42),
                api_cost_usd: 0.05,
            },
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["targets_linked"]["1"], true);
        assert_eq!(rows[0]["targets_linked"]["2"], false);
        assert_eq!(rows[0]["intersections_count"], 42);
    }

    #[test]
    fn flatten_page_intersection_object() {
        let map = flatten_page_intersection(Some(&serde_json::json!({
            "1": { "anchor": "x" },
            "3": true
        })));
        assert_eq!(map["1"], true);
        assert_eq!(map["3"], true);
    }
}
