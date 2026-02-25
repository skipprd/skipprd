use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidateFailureClass {
    SqlOrRuntime,
    SchemaOrPrecheck,
    Unknown,
}

pub fn classify_validate_failure(
    entered_from_precheck_failed: bool,
    last_validate_brief: Option<&str>,
    last_guard_reason: Option<&str>,
) -> ValidateFailureClass {
    if entered_from_precheck_failed {
        return ValidateFailureClass::SchemaOrPrecheck;
    }
    let mut hay = String::new();
    if let Some(b) = last_validate_brief {
        hay.push_str(b);
        hay.push('\n');
    }
    if let Some(r) = last_guard_reason {
        hay.push_str(r);
    }
    classify_from_text(&hay)
}

pub fn fingerprint_validate_observation(obs: &Value) -> String {
    let errs = obs
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .take(8)
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default();
    let class = if obs.get("compile_ok").and_then(|v| v.as_bool()).unwrap_or(false)
        && !obs.get("run_ok").and_then(|v| v.as_bool()).unwrap_or(false)
    {
        "runtime"
    } else if !obs.get("compile_ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        "compile"
    } else {
        "unknown"
    };
    format!("{}::{}", class, errs)
}

fn classify_from_text(haystack: &str) -> ValidateFailureClass {
    let t = haystack.to_ascii_lowercase();
    if t.trim().is_empty() {
        return ValidateFailureClass::Unknown;
    }
    if t.contains("precheck_failed")
        || t.contains("schema.yml")
        || t.contains(".yml")
        || t.contains(".yaml")
        || t.contains("yaml")
        || t.contains("schema contract")
        || t.contains("duplicate definition")
        || t.contains("duplicate definitions")
    {
        return ValidateFailureClass::SchemaOrPrecheck;
    }
    if t.contains("compilation error")
        || t.contains("database error")
        || t.contains("runtime error")
        || t.contains("column_not_found")
        || t.contains("unresolved column")
        || t.contains("syntax error")
        || t.contains("parse error")
    {
        return ValidateFailureClass::SqlOrRuntime;
    }
    // Default to SQL/runtime so deterministic repair targets SQL first when validate failed
    // but the upstream surface did not provide a structured class.
    ValidateFailureClass::SqlOrRuntime
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_validate_failure_prefers_schema_for_precheck_and_yaml() {
        assert_eq!(
            classify_validate_failure(true, None, None),
            ValidateFailureClass::SchemaOrPrecheck
        );
        assert_eq!(
            classify_validate_failure(
                false,
                Some("Error in models/schema.yml: duplicate definitions"),
                None
            ),
            ValidateFailureClass::SchemaOrPrecheck
        );
        assert_eq!(
            classify_validate_failure(
                false,
                Some("Compilation Error: syntax error near FROM"),
                None
            ),
            ValidateFailureClass::SqlOrRuntime
        );
    }

    #[test]
    fn fingerprint_validate_observation_is_deterministic() {
        let obs = serde_json::json!({
            "compile_ok": true,
            "run_ok": false,
            "errors": ["A", "B", "A", ""]
        });
        let fp = fingerprint_validate_observation(&obs);
        assert_eq!(fp, "runtime::A | B | A");
    }
}
