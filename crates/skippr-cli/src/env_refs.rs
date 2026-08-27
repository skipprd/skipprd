use serde_json::Value;

pub fn resolve_env_refs_in_json_value(value: &mut Value) -> Result<(), String> {
    fn resolve_env_ref(raw: &str, path: &str) -> Result<Option<String>, String> {
        let trimmed = raw.trim();
        if !(trimmed.starts_with("${") && trimmed.ends_with('}')) {
            return Ok(None);
        }
        if trimmed.len() <= 3 || trimmed[2..trimmed.len() - 1].contains("${") {
            return Err(format!("invalid environment reference '{raw}' at {path}"));
        }
        if trimmed != raw {
            return Err(format!(
                "environment reference '{raw}' at {path} must be the entire scalar value"
            ));
        }
        let var_name = &trimmed[2..trimmed.len() - 1];
        if var_name.trim().is_empty() {
            return Err(format!("empty environment reference at {path}"));
        }
        let env_value = std::env::var(var_name).map_err(|_| {
            format!(
                "skippr.yml references ${{{var_name}}} at {path}, but that environment variable is not set"
            )
        })?;
        if env_value.trim().is_empty() {
            return Err(format!(
                "skippr.yml references ${{{var_name}}} at {path}, but that environment variable is empty"
            ));
        }
        Ok(Some(env_value))
    }

    fn walk(value: &mut Value, path: String) -> Result<(), String> {
        match value {
            Value::String(raw) => {
                if let Some(resolved) = resolve_env_ref(raw, &path)? {
                    *raw = resolved;
                }
            }
            Value::Object(map) => {
                for (key, child) in map.iter_mut() {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk(child, child_path)?;
                }
            }
            Value::Array(items) => {
                for (idx, child) in items.iter_mut().enumerate() {
                    walk(child, format!("{path}[{idx}]"))?;
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
        Ok(())
    }

    walk(value, String::new())
}
