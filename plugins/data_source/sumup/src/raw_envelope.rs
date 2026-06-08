use chrono::Utc;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::privacy::{PrivacyConfig, PrivacyStats};

pub const RAW_SCHEMA_VERSION: &str = "1";

/// Recursively sort object keys for deterministic JSON serialization.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonicalize(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

pub fn payload_sha256(redacted: &Value) -> String {
    let canonical = canonicalize(redacted);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    format!("{:x}", digest)
}

pub fn build_raw_envelope_row(
    ingest_run_date: &str,
    merchant_code: &str,
    source_stream: &str,
    source_record_id: &str,
    source_uri: &str,
    redacted_payload: &Value,
) -> Value {
    json!({
        "ingest_run_date": ingest_run_date,
        "merchant_code": merchant_code,
        "source_stream": source_stream,
        "source_record_id": source_record_id,
        "source_uri": source_uri,
        "fetched_at": Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "payload_sha256": payload_sha256(redacted_payload),
        "schema_version": RAW_SCHEMA_VERSION,
    })
}

pub fn redact_and_envelope(
    privacy: &PrivacyConfig,
    ingest_run_date: &str,
    merchant_code: &str,
    source_stream: &str,
    source_record_id: &str,
    source_uri: &str,
    raw: &Value,
) -> (Value, PrivacyStats) {
    let mut redacted = raw.clone();
    let stats = privacy.redact_row(&mut redacted);
    let envelope = build_raw_envelope_row(
        ingest_run_date,
        merchant_code,
        source_stream,
        source_record_id,
        source_uri,
        &redacted,
    );
    (envelope, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::{PrivacyMode, PrivacyProfile};
    use serde_json::json;

    #[test]
    fn envelope_has_hash_not_payload() {
        let privacy = PrivacyConfig {
            mode: PrivacyMode::Profile,
            profile: PrivacyProfile::UpfoundrySafe,
            ..Default::default()
        };
        let raw = json!({
            "id": "tx-1",
            "email": "secret@example.com",
            "amount": 9.99
        });
        let (row, _) = redact_and_envelope(
            &privacy,
            "2024-06-01",
            "MH4H92C7",
            "transactions",
            "tx-1",
            "sumup://MH4H92C7/transactions/tx-1",
            &raw,
        );
        assert_eq!(row["source_record_id"], "tx-1");
        assert_eq!(row["source_stream"], "transactions");
        assert_eq!(row["merchant_code"], "MH4H92C7");
        assert_eq!(row["schema_version"], RAW_SCHEMA_VERSION);
        assert!(row.get("payload").is_none());
        assert!(row.get("email").is_none());
        assert!(row["payload_sha256"].as_str().unwrap().len() == 64);
    }

    #[test]
    fn canonical_hash_is_stable() {
        let a = json!({"b": 2, "a": 1});
        let b = json!({"a": 1, "b": 2});
        assert_eq!(payload_sha256(&a), payload_sha256(&b));
    }
}
