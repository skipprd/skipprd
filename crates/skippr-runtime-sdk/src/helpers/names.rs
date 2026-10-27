/// Sanitize a namespace/field token the same way host ingest does for an empty
/// child-process config (`flatten_events = false`).
pub fn clean_field_name(field: &str, flatten_events: bool) -> String {
    let mut clean = field.to_string();
    if field.parse::<i32>().is_ok() {
        if flatten_events {
            clean = field.to_string();
        } else {
            clean = format!("item_{field}");
        }
    }
    clean = clean.to_lowercase();
    let mut out = String::with_capacity(clean.len());
    let mut prev_underscore = false;
    for ch in clean.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if ch == '_' {
                if prev_underscore {
                    continue;
                }
                prev_underscore = true;
            } else {
                prev_underscore = false;
            }
            out.push(ch);
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    let stripped = out.trim_start_matches(['1', '2', '3', '4', '5', '6', '7', '8', '9']);
    stripped.trim_matches('_').to_string()
}

pub fn storage_namespace(namespace: &str) -> String {
    clean_field_name(namespace, false)
}

pub fn storage_path_token(raw: &str) -> String {
    let lower = raw.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut prev_underscore = false;
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if ch == '_' {
                if prev_underscore {
                    continue;
                }
                prev_underscore = true;
            } else {
                prev_underscore = false;
            }
            out.push(ch);
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    out.trim_matches('_').to_string()
}

pub fn storage_partition(partition: &str) -> String {
    if partition.is_empty() {
        return String::new();
    }
    if partition.contains('/') {
        partition
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(|segment| {
                if let Some((key, value)) = segment.split_once('=') {
                    format!(
                        "{}={}",
                        clean_field_name(key, false),
                        storage_path_token(value)
                    )
                } else {
                    clean_field_name(segment, false)
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    } else {
        storage_path_token(partition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_namespace_is_snake_case() {
        assert_eq!(storage_namespace("Bike Hire"), "bike_hire");
    }

    #[test]
    fn numeric_token_gets_item_prefix_when_not_flattened() {
        assert_eq!(clean_field_name("2024", false), "item_2024");
        // Leading 1-9 are stripped after sanitise; flatten still starts with digits.
        assert_eq!(clean_field_name("2024", true), "024");
    }
}
