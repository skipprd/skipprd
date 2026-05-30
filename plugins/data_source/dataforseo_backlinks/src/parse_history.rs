use serde_json::{Map, Value};

use crate::entity::EntityParseContext;
use crate::parse_util::{copy_fields, copy_json_fields};

const HISTORY_SCALAR_FIELDS: &[&str] = &[
    "type",
    "date",
    "rank",
    "backlinks",
    "new_backlinks",
    "lost_backlinks",
    "new_referring_domains",
    "lost_referring_domains",
    "crawled_pages",
    "internal_links_count",
    "external_links_count",
    "broken_backlinks",
    "broken_pages",
    "referring_domains",
    "referring_domains_nofollow",
    "referring_main_domains",
    "referring_main_domains_nofollow",
    "referring_ips",
    "referring_subnets",
    "referring_pages",
    "referring_pages_nofollow",
    "target_spam_score",
];

const HISTORY_JSON_FIELDS: &[&str] = &[
    "info",
    "referring_links_tld",
    "referring_links_types",
    "referring_links_attributes",
    "referring_links_platform_types",
    "referring_links_semantic_locations",
    "referring_links_countries",
];

pub fn parse_history_items(
    items: &[Value],
    entity: &EntityParseContext<'_>,
    api_cost_usd: f64,
) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| parse_history_item(item, entity, api_cost_usd))
        .collect()
}

fn parse_history_item(
    item: &Value,
    entity: &EntityParseContext<'_>,
    api_cost_usd: f64,
) -> Option<Value> {
    let history_date = item.get("date").and_then(|v| v.as_str())?;
    let mut row = Map::new();
    entity.apply_envelope(&mut row);
    row.insert("history_date".into(), serde_json::json!(history_date));
    copy_fields(&mut row, item, HISTORY_SCALAR_FIELDS);
    copy_json_fields(&mut row, item, HISTORY_JSON_FIELDS);
    row.insert("api_cost_usd".into(), serde_json::json!(api_cost_usd));
    Some(Value::Object(row))
}
