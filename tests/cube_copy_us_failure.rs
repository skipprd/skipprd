//! Reproduces the CloudCycle US staging COPY failure against EU success.
//!
//! Live hosts run skippr 16.0.0 with the same YAML (no `type: copy`, so ingest):
//! `flatten_events: false`, `namespace_fields: detail-type`,
//! `batch_time_fields: time`, `batch_partition_fields: detail.rmc`.
//!
//! US `cube_events_json_to_partitioned` exited 1 with:
//! `queued ingest task panicked` at `src/helpers/mod.rs:796`
//! (`begin <= end (1 <= 0) when slicing`) while compacting the
//! `cube_events/cube_events_json_to_partitioned/` latch prefix.
//! EU completed `namespaces=66` into `{detail-type}/` dirs and is still healthy.

use std::collections::HashMap;

use serde_json::json;
use serial_test::serial;
use skipprd::helpers::configuration::Config;
use skipprd::helpers::Helpers;

fn cube_copy_namespace(message: &serde_json::Value, cache: &mut HashMap<String, String>) -> String {
    Config::setenv("TRANSFORM_NAMESPACE_FIELDS", "detail-type");
    Helpers::parse_namespace_field(
        &Config::new(),
        message,
        "cube_events_json_to_partitioned".to_string(),
        cache,
    )
}

#[test]
#[serial]
fn eu_style_eventbridge_envelope_namespaces_by_detail_type() {
    let mut cache = HashMap::new();
    let docket = json!({
        "version": "0",
        "id": "eu-1",
        "detail-type": "cloudcycle-docket",
        "time": "2024-10-10T10:00:00Z",
        "detail": {"docket_rmc": "Cemex", "docket_id": "Cemex--1"}
    });
    let prediction = json!({
        "version": "0",
        "id": "eu-2",
        "detail-type": "consistence-prediction",
        "time": "2024-10-10T11:00:00Z",
        "detail": {"rmc": "Cemex"}
    });
    assert_eq!(
        cube_copy_namespace(&docket, &mut cache),
        "cloudcycle_docket"
    );
    assert_eq!(
        cube_copy_namespace(&prediction, &mut cache),
        "consistence_prediction"
    );
}

#[test]
#[serial]
fn us_style_raw_docket_envelope_namespaces_to_raw_docket() {
    let mut cache = HashMap::new();
    let envelope = json!({
        "version": "0",
        "id": "us-1",
        "detail-type": "raw-docket",
        "time": "2026-10-01T12:00:00Z",
        "detail": {"raw_docket": "{\"ticket\":1}", "rmc": "tilcon"}
    });
    assert_eq!(cube_copy_namespace(&envelope, &mut cache), "raw_docket");
}

#[test]
#[serial]
fn unwrapped_then_envelope_does_not_latch_later_records() {
    let mut cache = HashMap::new();
    let unwrapped = json!({"raw_docket": "{}", "rmc": "tilcon"});
    let envelope = json!({
        "detail-type": "raw-docket",
        "time": "2026-10-01T12:00:00Z",
        "detail": {"raw_docket": "{}", "rmc": "tilcon"}
    });
    assert_eq!(
        cube_copy_namespace(&unwrapped, &mut cache),
        "cube_events_json_to_partitioned"
    );
    assert_eq!(cube_copy_namespace(&envelope, &mut cache), "raw_docket");
}

#[test]
fn quote_only_time_does_not_panic_ingest_helpers() {
    let envelope = json!({
        "detail-type": "raw-docket",
        "time": "\"",
        "detail": {"raw_docket": "{}", "rmc": "tilcon"}
    });
    assert_eq!(
        Helpers::parse_time_field_with_fields(&envelope, "time"),
        None
    );
    for poison in ["\"", "'", "`"] {
        assert!(Helpers::parse_date_from_string(poison, "%Y-%m-%dT%H:%M:%SZ").is_err());
    }
}
