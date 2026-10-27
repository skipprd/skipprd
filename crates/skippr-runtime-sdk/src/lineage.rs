pub fn default_nullable() -> bool {
    true
}

/// Derive a deterministic positive 31-bit field id from a namespace and path.
pub fn deterministic_field_id(namespace: &str, output_path: &[String]) -> i32 {
    let mut input = String::with_capacity(namespace.len() + output_path.len() * 16);
    input.push_str(namespace);
    input.push('\0');
    input.push_str(&output_path.join("."));
    let digest = md5::compute(input.as_bytes());
    let raw = u32::from_be_bytes([digest.0[0], digest.0[1], digest.0[2], digest.0[3]]);
    (raw & 0x7fff_ffff) as i32
}

#[cfg(test)]
mod tests {
    use super::deterministic_field_id;

    #[test]
    fn field_id_is_stable_and_positive() {
        let id = deterministic_field_id("orders", &["id".to_string()]);
        assert!(id > 0);
        assert_eq!(id, deterministic_field_id("orders", &["id".to_string()]));
    }
}
