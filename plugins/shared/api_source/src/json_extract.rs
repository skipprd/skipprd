use serde_json::Value;

/// Extract row objects from a JSON API response using a dotted path to the rows array.
pub fn json_rows_from_response(body: &Value, rows_path: &[&str]) -> Vec<Value> {
    let mut current = body;
    for segment in rows_path {
        current = match current.get(*segment) {
            Some(v) => v,
            None => return vec![],
        };
    }
    current
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.is_object())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_rows_array() {
        let body = json!({"reports": [{"date": "20240101"}, {"date": "20240102"}]});
        let rows = json_rows_from_response(&body, &["reports"]);
        assert_eq!(rows.len(), 2);
    }
}
