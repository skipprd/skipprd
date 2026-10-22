use serde_json::{json, Map, Value};

pub const TARGET: &str = "skipprd::perf";
pub const PREFIX: &str = "PERF_EVENT ";

pub fn event_line(phase: &str, fields: Value) -> String {
    let mut obj = match fields {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("value".into(), other);
            map
        }
    };
    obj.insert("phase".into(), json!(phase));
    format!("{PREFIX}{}", Value::Object(obj))
}

pub fn emit(phase: &str, fields: Value) {
    tracing::info!(target: TARGET, "{}", event_line(phase, fields));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_line_is_prefixed_json_with_phase() {
        let line = event_line(
            "ingest_profile",
            json!({
                "elapsed_ms": 12,
                "rows": 100,
                "exact_arrow_rows": 100,
                "exact_fallback_rows": 0
            }),
        );
        assert!(line.starts_with(PREFIX), "{line}");
        let parsed: Value = serde_json::from_str(line.trim_start_matches(PREFIX)).unwrap();
        assert_eq!(parsed["phase"], "ingest_profile");
        assert_eq!(parsed["elapsed_ms"], 12);
        assert_eq!(parsed["rows"], 100);
        assert_eq!(parsed["exact_arrow_rows"], 100);
        assert_eq!(parsed["exact_fallback_rows"], 0);
    }
}
