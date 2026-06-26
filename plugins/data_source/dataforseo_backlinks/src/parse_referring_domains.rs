use serde_json::{json, Map, Value};

use skippr_plugin_shared_backlinks::{apply_referring_domain_row_ids, finalize_canonical_row};

use crate::entity::EntityParseContext;
use crate::parse_util::{copy_fields, copy_json_fields};

const REFERRING_DOMAIN_SCALAR_FIELDS: &[&str] = &[
    "type",
    "domain",
    "rank",
    "backlinks",
    "first_seen",
    "lost_date",
    "backlinks_spam_score",
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
];

const REFERRING_DOMAIN_JSON_FIELDS: &[&str] = &[
    "referring_links_tld",
    "referring_links_types",
    "referring_links_attributes",
    "referring_links_platform_types",
    "referring_links_semantic_locations",
    "referring_links_countries",
];

pub fn parse_referring_domain_items(
    items: &[Value],
    entity: &EntityParseContext<'_>,
    api_cost_usd: f64,
) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| parse_referring_domain_item(item, entity, api_cost_usd))
        .collect()
}

fn parse_referring_domain_item(
    item: &Value,
    entity: &EntityParseContext<'_>,
    api_cost_usd: f64,
) -> Option<Value> {
    let domain = item.get("domain").and_then(|v| v.as_str())?;
    let mut row = Map::new();
    entity.apply_envelope(&mut row);
    row.insert("domain".into(), serde_json::json!(domain));
    copy_fields(&mut row, item, REFERRING_DOMAIN_SCALAR_FIELDS);
    copy_json_fields(&mut row, item, REFERRING_DOMAIN_JSON_FIELDS);
    row.insert("api_cost_usd".into(), serde_json::json!(api_cost_usd));
    apply_referring_domain_row_ids(&mut row, domain);
    finalize_canonical_row(&mut row);
    Some(Value::Object(row))
}
