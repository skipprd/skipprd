use serde_json::{json, Map, Value};
use skippr_plugin_shared_link_graph::{anchor_id, domain_id, edge_id, url_id};

/// Console queries accept `target` and legacy `primary`.
pub fn canonical_entity_kind(entity_kind: &str) -> &str {
    if entity_kind == "primary" {
        "target"
    } else {
        entity_kind
    }
}

pub fn apply_entity_kind(row: &mut Map<String, Value>) {
    if let Some(kind) = row.get("entity_kind").and_then(Value::as_str) {
        row.insert("entity_kind".into(), json!(canonical_entity_kind(kind)));
    }
}

pub fn apply_backlink_row_ids(row: &mut Map<String, Value>, url_from: &str, url_to: &str) {
    let from_id = url_id(url_from);
    let to_id = url_id(url_to);
    row.insert("url_from_id".into(), json!(from_id));
    row.insert("url_to_id".into(), json!(to_id));
    let edge = edge_id(url_from, url_to, "", "");
    row.insert("edge_id".into(), json!(hex::encode(edge)));
}

pub fn apply_referring_domain_row_ids(row: &mut Map<String, Value>, domain: &str) {
    let source_domain_id = domain_id(domain);
    row.insert("source_domain_id".into(), json!(source_domain_id));
    if let Some(count) = row.get("backlinks").and_then(Value::as_i64) {
        row.insert("backlink_count".into(), json!(count));
    } else if let Some(count) = row.get("backlinks").and_then(Value::as_u64) {
        row.insert("backlink_count".into(), json!(count));
    }
}

pub fn apply_anchor_row_ids(row: &mut Map<String, Value>, anchor: &str) {
    let id = anchor_id(anchor);
    row.insert("anchor_id".into(), json!(id));
    if let Some(count) = row.get("backlinks").and_then(Value::as_i64) {
        row.insert("backlink_count".into(), json!(count));
    } else if let Some(count) = row.get("backlinks").and_then(Value::as_u64) {
        row.insert("backlink_count".into(), json!(count));
    }
}

pub fn apply_history_row_ids(
    row: &mut Map<String, Value>,
    site: &str,
    target: &str,
    history_date: &str,
) {
    let edge = edge_id(site, target, "history", history_date);
    row.insert("edge_id".into(), json!(hex::encode(edge)));
    row.insert("first_seen".into(), json!(history_date));
    row.insert("last_seen".into(), json!(history_date));
    row.insert("state".into(), json!("snapshot"));
}

pub fn finalize_canonical_row(row: &mut Map<String, Value>) {
    apply_entity_kind(row);
}
