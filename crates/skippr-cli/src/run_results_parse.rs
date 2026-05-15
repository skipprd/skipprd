//! Parse dbt `target/run_results.json` for `skippr test run` reporting.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedDbtRunResult {
    pub unique_id: String,
    pub status: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub failures: Option<u64>,
    #[serde(default)]
    pub compiled_path: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

/// Extract per-node test outcomes from dbt `run_results.json`.
pub fn parse_run_results_json(text: &str) -> Result<Vec<ParsedDbtRunResult>, String> {
    let v: Value =
        serde_json::from_str(text).map_err(|e| format!("invalid run_results json: {e}"))?;
    let results = v
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or_else(|| "run_results.json missing results array".to_string())?;
    let mut out = Vec::new();
    for row in results {
        let unique_id = row
            .get("unique_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if unique_id.is_empty() {
            continue;
        }
        let status = row
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string();
        let message = row
            .get("message")
            .and_then(|m| {
                if m.is_null() {
                    None
                } else {
                    Some(match m {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                }
            })
            .filter(|s| !s.trim().is_empty());
        let failures = row.get("failures").and_then(|x| x.as_u64());
        let compiled_path = row
            .get("compiled_path")
            .or_else(|| row.get("compiled_sql_path"))
            .and_then(|x| x.as_str())
            .map(str::to_string);
        let path = row.get("path").and_then(|x| x.as_str()).map(str::to_string);
        out.push(ParsedDbtRunResult {
            unique_id,
            status,
            message,
            failures,
            compiled_path,
            path,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
  "metadata": { "dbt_schema_version": "https://schemas.getdbt.com/dbt/run-results/v5.json" },
  "results": [
    {
      "unique_id": "test.demo.not_null_orders_id",
      "status": "pass",
      "message": null,
      "failures": 0,
      "path": "models/orders.yml",
      "compiled_path": "target/compiled/.../not_null_orders_id.sql"
    },
    {
      "unique_id": "test.demo.unique_orders_id",
      "status": "fail",
      "message": "Got 2 results, configured to fail if != 0",
      "failures": 2,
      "path": "models/orders.yml"
    }
  ]
}"#;

    #[test]
    fn parses_run_results_fixture() {
        let rows = parse_run_results_json(FIXTURE).expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].unique_id, "test.demo.not_null_orders_id");
        assert_eq!(rows[0].status, "pass");
        assert_eq!(rows[0].failures, Some(0));
        assert_eq!(
            rows[0].compiled_path.as_deref(),
            Some("target/compiled/.../not_null_orders_id.sql")
        );
        assert_eq!(rows[1].status, "fail");
        assert_eq!(rows[1].failures, Some(2));
        assert!(rows[1].message.as_ref().unwrap().contains("Got 2"));
    }
}
