use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FieldStats {
    pub total: u64,
    pub nulls: u64,
    pub min_numeric: Option<f64>,
    pub max_numeric: Option<f64>,
    pub min_len: Option<u64>,
    pub max_len: Option<u64>,
    pub approx_distinct_hll: Option<Vec<u8>>, // placeholder serialized state
    pub last_updated_epoch_ms: u64,
}

impl FieldStats {
    pub fn update_value(&mut self, value: &serde_json::Value) {
        self.total = self.total.saturating_add(1);
        if value.is_null() { self.nulls = self.nulls.saturating_add(1); return; }
        match value {
            serde_json::Value::Number(n) => {
                if let Some(f) = n.as_f64() {
                    self.min_numeric = Some(self.min_numeric.map(|v| v.min(f)).unwrap_or(f));
                    self.max_numeric = Some(self.max_numeric.map(|v| v.max(f)).unwrap_or(f));
                }
            }
            serde_json::Value::String(s) => {
                let len = s.len() as u64;
                self.min_len = Some(self.min_len.map(|v| v.min(len)).unwrap_or(len));
                self.max_len = Some(self.max_len.map(|v| v.max(len)).unwrap_or(len));
            }
            _ => {}
        }
        self.last_updated_epoch_ms = current_millis();
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NamespaceStats {
    pub namespace: String,
    pub fields: HashMap<String, FieldStats>,
    pub last_updated_epoch_ms: u64,
}

impl NamespaceStats {
    pub fn new(namespace: &str) -> Self {
        Self { namespace: namespace.to_string(), fields: HashMap::new(), last_updated_epoch_ms: current_millis() }
    }

    pub fn update_field(&mut self, field: &str, value: &serde_json::Value) {
        let entry = self.fields.entry(field.to_string()).or_insert_with(FieldStats::default);
        entry.update_value(value);
        self.last_updated_epoch_ms = current_millis();
    }
}

fn current_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn updates_numeric_and_string_bounds() {
        let mut ns = NamespaceStats::new("ns1");
        ns.update_field("a", &json!(3));
        ns.update_field("a", &json!(1));
        ns.update_field("a", &json!(5));
        ns.update_field("s", &json!("hi"));
        ns.update_field("s", &json!("hello"));
        let a = ns.fields.get("a").unwrap();
        assert_eq!(a.total, 3);
        assert_eq!(a.nulls, 0);
        assert_eq!(a.min_numeric, Some(1.0));
        assert_eq!(a.max_numeric, Some(5.0));
        let s = ns.fields.get("s").unwrap();
        assert_eq!(s.min_len, Some(2));
        assert_eq!(s.max_len, Some(5));
    }
}


