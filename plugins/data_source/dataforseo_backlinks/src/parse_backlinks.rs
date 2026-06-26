use serde_json::{json, Map, Value};

use skippr_plugin_shared_backlinks::{apply_backlink_row_ids, finalize_canonical_row};

use crate::config::BacklinkJob;
use crate::config::MAX_OFFSET;
use crate::entity::EntityParseContext;
use crate::parse_util::copy_field;
use crate::target::normalize_target;

pub fn build_backlink_task(
    job: &BacklinkJob,
    normalized_target: &str,
    limit: u32,
    offset: u32,
    search_after_token: Option<&str>,
    rank_scale: Option<&str>,
) -> Value {
    let mut task = Map::new();
    task.insert("target".into(), json!(normalized_target));
    task.insert("limit".into(), json!(limit));
    if let Some(token) = search_after_token.filter(|t| !t.is_empty()) {
        task.insert("search_after_token".into(), json!(token));
    } else {
        task.insert("offset".into(), json!(offset));
    }
    if let Some(mode) = job.mode.as_deref().filter(|s| !s.is_empty()) {
        task.insert("mode".into(), json!(mode));
    }
    if let Some(status) = job
        .backlinks_status_type
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        task.insert("backlinks_status_type".into(), json!(status));
    }
    if let Some(filters) = &job.filters {
        task.insert("filters".into(), filters.clone());
    }
    if let Some(order_by) = &job.order_by {
        task.insert("order_by".into(), json!(order_by));
    }
    if let Some(v) = job.include_subdomains {
        task.insert("include_subdomains".into(), json!(v));
    }
    if let Some(v) = job.exclude_internal_backlinks {
        task.insert("exclude_internal_backlinks".into(), json!(v));
    }
    if let Some(scale) = rank_scale.filter(|s| !s.is_empty()) {
        task.insert("rank_scale".into(), json!(scale));
    }
    Value::Object(task)
}

pub fn parse_backlinks_items(items: &[Value], ctx: &BacklinkParseContext) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| parse_backlink_item(item, ctx))
        .collect()
}

pub struct BacklinkParseContext<'a> {
    pub entity: EntityParseContext<'a>,
    pub job_tag: &'a str,
    pub backlinks_status_type: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub api_cost_usd: f64,
}

fn parse_backlink_item(item: &Value, ctx: &BacklinkParseContext<'_>) -> Option<Value> {
    let url_from = item.get("url_from").and_then(|v| v.as_str())?;
    let url_to = item.get("url_to").and_then(|v| v.as_str())?;
    let mut row = Map::new();
    ctx.entity.apply_envelope(&mut row);
    row.insert("job_tag".into(), json!(ctx.job_tag));
    row.insert("url_from".into(), json!(url_from));
    row.insert("url_to".into(), json!(url_to));

    for key in [
        "domain_from",
        "page_from_rank",
        "domain_from_rank",
        "domain_from_country",
        "domain_to",
        "url_to_status_code",
        "anchor",
        "dofollow",
        "item_type",
        "attributes",
        "is_new",
        "is_lost",
        "is_broken",
        "rank",
        "backlink_spam_score",
        "links_count",
        "first_seen",
        "last_seen",
    ] {
        copy_field(&mut row, item, key);
    }

    if let Some(v) = item.get("indirect_link_path") {
        row.insert("indirect_link_path".into(), v.clone());
    }
    if let Some(v) = item.get("ranked_keywords_info") {
        row.insert("ranked_keywords_info".into(), v.clone());
    }

    if let Some(v) = ctx.backlinks_status_type {
        row.insert("backlinks_status_type".into(), json!(v));
    }
    if let Some(v) = ctx.mode {
        row.insert("mode".into(), json!(v));
    }
    row.insert("api_cost_usd".into(), json!(ctx.api_cost_usd));

    apply_backlink_row_ids(&mut row, url_from, url_to);
    finalize_canonical_row(&mut row);

    Some(Value::Object(row))
}

pub fn next_pagination_state(
    limit: u32,
    offset: u32,
    items_count: u32,
    search_after_token: Option<String>,
    pages_completed: u32,
    max_pages: u32,
) -> PaginationAdvance {
    if pages_completed >= max_pages {
        return PaginationAdvance::Done;
    }
    if items_count < limit {
        return PaginationAdvance::Done;
    }
    if let Some(token) = search_after_token.filter(|t| !t.is_empty()) {
        return PaginationAdvance::ContinueWithToken {
            search_after_token: token,
            pages_completed: pages_completed + 1,
        };
    }
    let next_offset = offset.saturating_add(limit);
    if next_offset > MAX_OFFSET {
        return PaginationAdvance::Done;
    }
    PaginationAdvance::ContinueWithOffset {
        offset: next_offset,
        pages_completed: pages_completed + 1,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaginationAdvance {
    Done,
    ContinueWithOffset {
        offset: u32,
        pages_completed: u32,
    },
    ContinueWithToken {
        search_after_token: String,
        pages_completed: u32,
    },
}

pub fn prepare_backlink_target(job: &BacklinkJob) -> Result<String, std::io::Error> {
    normalize_target(&job.target).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_backlinks_items_maps_fixture_fields() {
        let items = vec![serde_json::json!({
            "url_from": "https://referrer.com/page",
            "url_to": "https://example.com/",
            "domain_from": "referrer.com",
            "anchor": "Example",
            "dofollow": true,
            "rank": 120
        })];
        let entity = crate::entity::SyncEntity {
            site: "example".into(),
            target: "example.com".into(),
            entity_kind: crate::entity::EntityKind::Primary,
            competitor_name: None,
            backlink_jobs: vec![],
        };
        let rows = parse_backlinks_items(
            &items,
            &BacklinkParseContext {
                entity: entity.parse_context("2024-06-01"),
                job_tag: "main",
                backlinks_status_type: Some("live"),
                mode: Some("one_per_domain"),
                api_cost_usd: 0.02,
            },
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["url_from"], "https://referrer.com/page");
        assert_eq!(rows[0]["target"], "example.com");
        assert_eq!(rows[0]["entity_kind"], "target");
        assert_eq!(rows[0]["rank"], 120);
    }

    #[test]
    fn pagination_stops_at_max_pages() {
        let adv = next_pagination_state(100, 0, 100, None, 5, 5);
        assert_eq!(adv, PaginationAdvance::Done);
    }

    #[test]
    fn search_after_token_roundtrip() {
        let adv = next_pagination_state(100, 20_000, 100, Some("token-abc".into()), 1, 10);
        assert_eq!(
            adv,
            PaginationAdvance::ContinueWithToken {
                search_after_token: "token-abc".into(),
                pages_completed: 2,
            }
        );
    }
}
