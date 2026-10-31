use crate::discover::SkipprDataType;
use crate::discover::schema_alter::is_iceberg_legal_promote;
use crate::helpers::Helpers;
use crate::lineage::deterministic_field_id;
use serde_json::{Map, Value, json};
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
fn e_schema_arrow_decimal_15_2_rescales_to_38_9() {
    use crate::ingest::schema_arrow::{
        SKIPPR_DECIMAL_PRECISION, SKIPPR_DECIMAL_SCALE, coerce_record_batch_to_skippr,
        skippr_schema_from_source,
    };
    use crate::serdes::decode::{DecodedIngest, decode_ingest};
    use crate::serdes::input_format::InputFormat;
    use crate::serdes::parquet::encode_parquet;
    use arrow::array::Decimal128Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    let arr = Decimal128Array::from(vec![Some(12345i128)])
        .with_precision_and_scale(15, 2)
        .unwrap();
    let schema = Arc::new(Schema::new(vec![Field::new(
        "l_extendedprice",
        DataType::Decimal128(15, 2),
        true,
    )]));
    let batch = RecordBatch::try_new(schema, vec![Arc::new(arr)]).unwrap();
    let bytes = encode_parquet(&[batch]).unwrap();
    let DecodedIngest::Arrow(batches) = decode_ingest(
        &crate::helpers::configuration::Config::new(),
        InputFormat::Parquet,
        &bytes,
    )
    .unwrap() else {
        panic!("parquet must be Arrow");
    };
    let skippr_schema = skippr_schema_from_source(&batches[0].schema()).unwrap();
    let coerced = coerce_record_batch_to_skippr(&batches[0], skippr_schema).unwrap();
    match coerced.schema().field(0).data_type() {
        DataType::Decimal128(p, s) => {
            assert_eq!(*p, SKIPPR_DECIMAL_PRECISION);
            assert_eq!(*s, SKIPPR_DECIMAL_SCALE);
        }
        other => panic!("expected Decimal128(38,9), got {other}"),
    }
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
fn e_nested_leaf_type_split_must_not_evolve_parent_record() {
    use crate::discover::{AnalyseSchema, SkipprDataType};
    use crate::helpers::configuration::Config;
    use crate::ingest::ingest::{ingest, set_value};
    use serde_json::json;
    use std::collections::HashMap;

    let mut child = crate::discover::Metadata::new().unwrap();
    child.determined_type = SkipprDataType::Decimal;
    child.out_field_name = "docket_aggregate_size_code".into();
    child.enabled = true;
    let mut children = HashMap::new();
    children.insert("docket_aggregate_size_code".into(), child);
    let mut parent = crate::discover::Metadata::new().unwrap();
    parent.determined_type = SkipprDataType::Record;
    parent.out_field_name = "detail".into();
    parent.fields = Box::new(children);
    let mut metadata = HashMap::new();
    metadata.insert("detail".into(), parent);
    let mut updated = "no".to_string();
    let resolved = set_value(
        &Config::new(),
        "record",
        "detail",
        &json!({"docket_aggregate_size_code": "20mm"}),
        None,
        None,
        &mut metadata,
        &mut updated,
        true,
        true,
    );
    let resolved =
        resolved.unwrap_or_else(|err| panic!("DST: leaf type-split must keep the row, got {err}"));
    assert!(
        metadata
            .get("detail")
            .unwrap()
            .fields
            .get("docket_aggregate_size_code_string")
            .is_some(),
        "DST: sibling must be a leaf, not detail_record"
    );
    assert_eq!(resolved.value["docket_aggregate_size_code_string"], "20mm");

    let disc = concat!(
        r#"{"version":"0","id":"disc-1","detail-type":"cloudcycle-docket","source":"com.cloudcycle","account":"1","time":"2024-10-10T10:00:00Z","region":"eu-west-1","resources":[],"detail":{"docket_aggregate_size_code":"20.00","docket_plant_code":"1234"}}"#
    );
    let mut discovered = HashMap::new();
    AnalyseSchema { i: 0 }.infer_json_schema(
        &Config::new(),
        &mut disc.to_string(),
        Some(1),
        &mut discovered,
        Some("cloudcycle_docket"),
    );
    let ns = discovered
        .keys()
        .find(|k| k.contains("docket") || k.as_str() == "default")
        .cloned()
        .or_else(|| discovered.keys().next().cloned())
        .expect("discovered namespace");
    let fields = discovered.get_mut(&ns).unwrap().fields.as_mut();
    if let Some(leaf) = fields
        .get_mut("detail")
        .and_then(|d| d.fields.get_mut("docket_aggregate_size_code"))
    {
        leaf.determined_type = SkipprDataType::Decimal;
    } else {
        panic!(
            "DST discover must nest detail.docket_aggregate_size_code, ns={ns} keys={:?}",
            fields.keys().collect::<Vec<_>>()
        );
    }
    let mut updated = "no".to_string();
    let later = json!({
        "version": "0",
        "id": "2cbc9030-adbe-afe2-b6be-c879f6eed5f5",
        "detail-type": "cloudcycle-docket",
        "source": "com.cloudcycle",
        "account": "1",
        "time": "2024-10-10T10:39:54Z",
        "region": "eu-west-1",
        "resources": [],
        "detail": {
            "docket_aggregate_size_code": "20mm",
            "docket_plant_code": "BWL"
        }
    });
    let row = ingest(&Config::new(), &later, fields, &ns, &mut updated, true)
        .unwrap_or_else(|err| panic!("DST ingest must not deadletter leaf coerce: {err}"));
    let obj = row.as_object().expect("DST flattened object");
    assert!(
        obj.keys().all(|k| !k.starts_with("detail_record_")),
        "DST must not evolve parent detail, row={row}"
    );
    assert!(
        obj.keys()
            .any(|k| k.ends_with("docket_aggregate_size_code_string")),
        "DST flatten must emit the leaf string sibling, row={row}"
    );
}

#[test]
fn e_array_of_records_leaf_type_split_denied() {
    use crate::discover::SkipprDataType;
    use crate::helpers::configuration::Config;
    use crate::ingest::deny::{ARRAY_MIXED_TYPES, IngestDenyError};
    use crate::ingest::ingest::set_value;
    use serde_json::json;
    use std::collections::HashMap;

    let mut leaf = crate::discover::Metadata::new().unwrap();
    leaf.determined_type = SkipprDataType::Decimal;
    leaf.out_field_name = "max_size_mm".into();
    leaf.enabled = true;
    let mut elem = crate::discover::Metadata::new().unwrap();
    elem.determined_type = SkipprDataType::Record;
    elem.out_field_name = "0".into();
    elem.fields.insert("max_size_mm".into(), leaf);
    let mut arr = crate::discover::Metadata::new().unwrap();
    arr.determined_type = SkipprDataType::Array;
    arr.determined_type_values = Some(SkipprDataType::Record);
    arr.out_field_name = "aggregate".into();
    arr.fields.insert("0".into(), elem);
    let mut metadata = HashMap::new();
    metadata.insert("aggregate".into(), arr);
    let mut updated = "no".to_string();
    let err = set_value(
        &Config::new(),
        "array",
        "aggregate",
        &json!([{"max_size_mm": "20mm"}]),
        None,
        None,
        &mut metadata,
        &mut updated,
        true,
        false,
    )
    .expect_err("DST: array-of-records leaf must deny type-split");
    let deny = err
        .downcast_ref::<IngestDenyError>()
        .expect("DST: ARRAY_MIXED_TYPES");
    assert_eq!(deny.code, ARRAY_MIXED_TYPES);
    assert!(
        metadata
            .get("aggregate")
            .unwrap()
            .fields
            .get("0")
            .unwrap()
            .fields
            .get("max_size_mm_string")
            .is_none()
    );
}

#[test]
fn e_primitive_array_mixed_types_denied() {
    use crate::discover::SkipprDataType;
    use crate::helpers::configuration::Config;
    use crate::ingest::deny::{ARRAY_MIXED_TYPES, IngestDenyError};
    use crate::ingest::ingest::ingest;
    use serde_json::json;
    use std::collections::HashMap;

    let mut arr = crate::discover::Metadata::new().unwrap();
    arr.determined_type = SkipprDataType::Array;
    arr.determined_type_values = Some(SkipprDataType::Integer);
    arr.out_field_name = "codes".into();
    arr.enabled = true;
    let mut metadata = HashMap::new();
    metadata.insert("codes".into(), arr);
    let mut updated = "no".to_string();
    let err = ingest(
        &Config::new(),
        &json!({"codes": [1234, "BWL"]}),
        &mut metadata,
        "default",
        &mut updated,
        false,
    )
    .expect_err("DST: primitive mixed array must deny");
    let deny = err
        .downcast_ref::<IngestDenyError>()
        .expect("DST: ARRAY_MIXED_TYPES");
    assert_eq!(deny.code, ARRAY_MIXED_TYPES);
    assert!(metadata.get("codes_0_string").is_none());
    assert!(!metadata.keys().any(|k| k.contains("codes_array")));
}

#[test]
fn e_flattened_array_index_mixed_leaf_denied() {
    use crate::discover::SkipprDataType;
    use crate::helpers::configuration::Config;
    use crate::ingest::deny::{ARRAY_MIXED_TYPES, IngestDenyError};
    use crate::ingest::ingest::ingest;
    use serde_json::json;
    use std::collections::HashMap;

    let mut leaf = crate::discover::Metadata::new().unwrap();
    leaf.determined_type = SkipprDataType::Integer;
    leaf.out_field_name = "rpm_min".into();
    leaf.enabled = true;
    let mut elem = crate::discover::Metadata::new().unwrap();
    elem.determined_type = SkipprDataType::Record;
    elem.out_field_name = "0".into();
    elem.enabled = true;
    elem.fields.insert("rpm_min".into(), leaf);
    let mut arr = crate::discover::Metadata::new().unwrap();
    arr.determined_type = SkipprDataType::Array;
    arr.determined_type_values = Some(SkipprDataType::Record);
    arr.out_field_name = "speed_pattern_stages".into();
    arr.enabled = true;
    arr.fields.insert("0".into(), elem);
    let mut children = HashMap::new();
    children.insert("speed_pattern_stages".into(), arr);
    let mut detail = crate::discover::Metadata::new().unwrap();
    detail.determined_type = SkipprDataType::Record;
    detail.out_field_name = "detail".into();
    detail.fields = Box::new(children);
    let mut metadata = HashMap::new();
    metadata.insert("detail".into(), detail);
    let mut updated = "no".to_string();
    let err = ingest(
        &Config::new(),
        &json!({"detail":{"speed_pattern_stages":[{"rpm_min":8},{"rpm_min":1.5}]}}),
        &mut metadata,
        "default",
        &mut updated,
        true,
    )
    .expect_err("DST: flatten mixed rpm_min must deny");
    let deny = err
        .downcast_ref::<IngestDenyError>()
        .expect("DST: ARRAY_MIXED_TYPES");
    assert_eq!(deny.code, ARRAY_MIXED_TYPES);
}

#[test]
fn e_leaf_split_then_fast_apply() {
    use crate::discover::SkipprDataType;
    use crate::helpers::configuration::Config;
    use crate::ingest::fast_ingest::fast_set_value;
    use crate::ingest::ingest::set_value;
    use serde_json::json;
    use std::collections::HashMap;

    let mut child = crate::discover::Metadata::new().unwrap();
    child.determined_type = SkipprDataType::Decimal;
    child.out_field_name = "docket_aggregate_size_code".into();
    child.enabled = true;
    let mut children = HashMap::new();
    children.insert("docket_aggregate_size_code".into(), child);
    let mut parent = crate::discover::Metadata::new().unwrap();
    parent.determined_type = SkipprDataType::Record;
    parent.out_field_name = "detail".into();
    parent.fields = Box::new(children);
    let mut metadata = HashMap::new();
    metadata.insert("detail".into(), parent);
    let mut updated = "no".to_string();
    set_value(
        &Config::new(),
        "record",
        "detail",
        &json!({"docket_aggregate_size_code": "20mm"}),
        None,
        None,
        &mut metadata,
        &mut updated,
        true,
        true,
    )
    .expect("DST: slow create");
    let applied = fast_set_value(
        &Config::new(),
        &SkipprDataType::Record,
        "detail",
        &json!({"docket_aggregate_size_code": "10mm"}),
        &metadata,
        Some(true),
        true,
    )
    .expect("DST: fast apply after publish");
    assert_eq!(applied.value["docket_aggregate_size_code_string"], "10mm");
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
