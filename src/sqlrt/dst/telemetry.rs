use crate::telemetry::{clamp_catalog_stem, PluginRole, TelemetryPayload};

#[test]
fn j_telemetry_payload_has_no_pipeline_name() {
    let payload = TelemetryPayload {
        schema: 1,
        install_id: "00000000-0000-4000-8000-000000000001".into(),
        day: "2026-11-08".into(),
        version: "0.1.0".into(),
        os: "macos-aarch64".into(),
        command: crate::telemetry::Command::Sync,
        pipeline_type: "ingest".into(),
        source: clamp_catalog_stem("Postgres", PluginRole::Source),
        sink: clamp_catalog_stem("custom_lake", PluginRole::Sink),
        schema_sink: clamp_catalog_stem("Glue", PluginRole::SchemaSink),
        flatten: 0,
        input_format: "json".into(),
        output_format: "native".into(),
        error_class: crate::telemetry::ErrorClass::Ok,
        runs: 1,
        rows: 0,
        bytes: 0,
        duration_ms: 0,
        namespaces: 0,
        fields: 0,
        evolutions: 0,
        deadletters: 0,
        flight_fail: 0,
        iceberg_cas: 0,
        compact_duration_ns: 0,
        compact_cycles: 0,
        apply_duration_ns: 0,
        apply_calls: 0,
    };
    assert_eq!(payload.source, "postgres");
    assert_eq!(payload.sink, "other");
    assert_eq!(payload.schema_sink, "glue");
    let encoded = serde_json::to_string(&payload).unwrap();
    assert!(
        !encoded.contains("pipeline_name"),
        "pipeline names are unrepresentable: {encoded}"
    );
    assert!(!encoded.contains("bikehire"));
}
