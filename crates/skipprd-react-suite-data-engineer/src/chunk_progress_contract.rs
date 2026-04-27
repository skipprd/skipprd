use std::collections::BTreeSet;

pub fn enforce_chunk_contract(
    items: &[String],
    max_chunk_size: usize,
    kind: &str,
) -> Result<(), String> {
    if items.is_empty() {
        return Err(format!("{kind}: chunk contract violation: empty batch"));
    }
    if items.len() > max_chunk_size {
        return Err(format!(
            "{kind}: chunk contract violation: batch size {} exceeds max {}",
            items.len(),
            max_chunk_size
        ));
    }

    let mut seen = BTreeSet::new();
    for it in items.iter() {
        let t = it.trim();
        if t.is_empty() {
            return Err(format!(
                "{kind}: chunk contract violation: batch contains empty identifier"
            ));
        }
        if !seen.insert(t.to_string()) {
            return Err(format!(
                "{kind}: chunk contract violation: duplicate batch identifier '{}'",
                t
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::enforce_chunk_contract;

    #[test]
    fn enforce_chunk_contract_rejects_too_large_batch() {
        let batch = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
            "e".to_string(),
            "f".to_string(),
        ];
        let err =
            enforce_chunk_contract(&batch, crate::plan_progress::MAX_BATCH_SIZE, "cleanse_sql")
                .expect_err("expected error");
        assert!(err.contains("exceeds max"));
    }
}
