use serde_json::{Map, Value};

use skippr_plugin_shared_backlinks::{apply_anchor_row_ids, finalize_canonical_row};

use crate::entity::EntityParseContext;
use crate::parse_util::copy_fields;

const ANCHOR_SCALAR_FIELDS: &[&str] = &[
    "type",
    "anchor",
    "rank",
    "backlinks",
    "first_seen",
    "lost_date",
    "backlinks_spam_score",
    "broken_backlinks",
    "broken_pages",
    "referring_domains",
    "referring_main_domains",
    "referring_pages",
];

pub fn parse_anchor_items(
    items: &[Value],
    entity: &EntityParseContext<'_>,
    api_cost_usd: f64,
) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| parse_anchor_item(item, entity, api_cost_usd))
        .collect()
}

fn parse_anchor_item(
    item: &Value,
    entity: &EntityParseContext<'_>,
    api_cost_usd: f64,
) -> Option<Value> {
    let anchor = item.get("anchor").and_then(|v| v.as_str())?;
    let mut row = Map::new();
    entity.apply_envelope(&mut row);
    row.insert("anchor".into(), serde_json::json!(anchor));
    copy_fields(&mut row, item, ANCHOR_SCALAR_FIELDS);
    row.insert("api_cost_usd".into(), serde_json::json!(api_cost_usd));
    apply_anchor_row_ids(&mut row, anchor);
    finalize_canonical_row(&mut row);
    Some(Value::Object(row))
}
