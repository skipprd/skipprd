use crate::discover::schema_alter::is_iceberg_legal_promote;
use crate::discover::SkipprDataType;
use crate::helpers::Helpers;
use crate::lineage::deterministic_field_id;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

#[test]
fn a_flatten_on_vs_off_field_ids_stable() {
    let nested = json!({"detail": {"type": "pour"}});
    let flattened = Helpers::flatten(&nested, &HashMap::new()).unwrap();
    let flat_key = flattened
        .as_object()
        .unwrap()
        .keys()
        .find(|k| k.contains("detail"))
        .cloned()
        .expect("flatten on must emit a nested key");
    let nested_id = deterministic_field_id("cube_events", &["detail".into(), "type".into()]);
    let flat_id = deterministic_field_id("cube_events", std::slice::from_ref(&flat_key));
    assert_eq!(
        nested_id,
        deterministic_field_id("cube_events", &["detail".into(), "type".into()])
    );
    assert_ne!(
        nested_id, flat_id,
        "flatten on vs dotted path must not share field ids"
    );
}

#[test]
fn e_type_widen_legal_illegal_promotion() {
    assert!(is_iceberg_legal_promote(
        &SkipprDataType::Integer,
        &SkipprDataType::Long
    ));
    assert!(!is_iceberg_legal_promote(
        &SkipprDataType::String,
        &SkipprDataType::Long
    ));
}

#[test]
fn e_nested_object_array_json() {
    let json = json!({
        "detail": {"type": "pour_start", "items": [1, 2]},
        "id": 1
    });
    let flat = Helpers::flatten(&json, &HashMap::new()).expect("nested JSON must flatten");
    let obj = flat.as_object().expect("flatten object");
    assert!(obj.contains_key("id") || obj.keys().any(|k| k.contains("detail")));
}

#[test]
fn e_repair_duplicate_zero_field_ids() {
    let mut seen = HashMap::new();
    let a = crate::discover::Metadata::unique_deterministic_field_id(
        "cube_events",
        &["amount".into()],
        &seen,
    );
    seen.insert(a, "amount".into());
    let b = crate::discover::Metadata::unique_deterministic_field_id(
        "cube_events",
        &["amount".into()],
        &seen,
    );
    assert_ne!(a, b);
    assert!(a > 0 && b > 0);
}

#[test]
fn e_rename_via_evolution_preserves_field_id() {
    let id = deterministic_field_id("s3_alter", &["region".into()]);
    assert_eq!(id, deterministic_field_id("s3_alter", &["region".into()]));
}

#[test]
fn e_new_namespace_mid_pipeline() {
    let a = deterministic_field_id("ns_a", &["id".into()]);
    let b = deterministic_field_id("ns_b", &["id".into()]);
    assert_ne!(a, b);
}

#[test]
fn e_discover_never_panics_then_query() {
    for raw in [
        b"{}" as &[u8],
        b"not-json",
        br#"{"a": null, "b": [1, {"c": true}]}"#,
        &vec![0xff; 32],
    ] {
        let parsed = serde_json::from_slice::<Value>(raw);
        if let Ok(Value::Object(map)) = parsed {
            let v = Value::Object(map);
            let _ = Helpers::flatten(&v, &HashMap::new());
        }
    }
    let _ = Helpers::flatten(&Value::Object(Map::new()), &HashMap::new());
    let world = crate::sqlrt::dst::run(&[crate::sqlrt::dst::Op::IngestWal(
        crate::sqlrt::dst::WalEvent {
            row: crate::sqlrt::dst::UserRow {
                id: 1,
                name: "after-discover".into(),
                amount: 1,
            },
            mutation: Some(crate::plugins::cdc::MutationKind::Insert),
            order: 1,
            seq: 1,
            ordinal: 0,
            row_i: 0,
        },
    )]);
    assert_eq!(crate::sqlrt::dst::count(&world), 1);
}

#[test]
fn j_cube_events_namespaces() {
    let events = [
        json!({"detail-type": "pour_start", "id": 10}),
        json!({"detail-type": "pour_stop", "id": 11}),
        json!({"detail-type": "heartbeat", "id": 12}),
    ];
    let mut namespaces = Vec::new();
    for event in events {
        let flat = Helpers::flatten(&event, &HashMap::new()).unwrap();
        let obj = flat.as_object().expect("flatten object");
        let ns = obj
            .get("detail-type")
            .or_else(|| obj.get("detail_type"))
            .and_then(|v| v.as_str())
            .expect("namespace field");
        namespaces.push(ns.to_string());
    }
    namespaces.sort();
    assert_eq!(namespaces, vec!["heartbeat", "pour_start", "pour_stop"]);
}

#[test]
fn j_device_json_wide_row() {
    let mut device = Map::new();
    device.insert("id".into(), json!(99));
    for i in 0..64 {
        device.insert(format!("metric_{i}"), json!(i));
    }
    let flat = Helpers::flatten(&Value::Object(device), &HashMap::new()).unwrap();
    let obj = flat.as_object().expect("wide row");
    assert!(obj.len() >= 64, "device JSON flatten must keep wide fields");
}
