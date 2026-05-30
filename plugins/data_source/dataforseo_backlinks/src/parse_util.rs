use serde_json::{Map, Value};

pub fn copy_field(row: &mut Map<String, Value>, item: &Value, key: &str) {
    if let Some(v) = item.get(key) {
        row.insert(key.to_string(), v.clone());
    }
}

pub fn copy_fields(row: &mut Map<String, Value>, item: &Value, keys: &[&str]) {
    for key in keys {
        copy_field(row, item, key);
    }
}

pub fn copy_json_fields(row: &mut Map<String, Value>, item: &Value, keys: &[&str]) {
    for key in keys {
        if let Some(v) = item.get(key) {
            if v.is_object() || v.is_array() {
                row.insert(key.to_string(), v.clone());
            }
        }
    }
}
