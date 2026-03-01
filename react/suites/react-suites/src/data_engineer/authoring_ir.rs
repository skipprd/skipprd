use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::data_engineer::tools::files_tool;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelIntent {
    pub sql: String,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub columns: Vec<ColumnIntent>,
    #[serde(default)]
    pub tests: TestsIntent,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnIntent {
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestsIntent {
    #[serde(default)]
    pub not_null: Vec<String>,
}

pub fn compile_sql_first_draft(sql: &str, notes: &[String]) -> Result<ModelIntent, String> {
    let normalized_sql = sql.trim();
    if normalized_sql.is_empty() {
        return Err("authoring_ir: sql is empty".to_string());
    }

    let has_wildcard_projection = {
        let low = normalized_sql.to_ascii_lowercase();
        low.contains("select *") || low.contains(".* from")
    };
    let cols = match files_tool::extract_final_select_output_columns(normalized_sql) {
        Ok(v) => v,
        Err(e) => {
            // `extract_final_select_output_columns` expects `FROM` to start on its own line.
            // SQL-first drafts can still be single-line; normalize once before failing.
            let lower = normalized_sql.to_ascii_lowercase();
            let patched = lower
                .rfind(" from ")
                .map(|idx| {
                    let mut s = normalized_sql.to_string();
                    s.replace_range(idx..idx + 6, "\nfrom ");
                    s
                })
                .unwrap_or_else(|| normalized_sql.to_string());
            match files_tool::extract_final_select_output_columns(&patched) {
                Ok(v) => v,
                Err(e2) => {
                    if has_wildcard_projection {
                        BTreeSet::new()
                    } else {
                        return Err(format!(
                            "authoring_ir: could not infer final output columns from sql: {}; fallback_error: {}",
                            e, e2
                        ));
                    }
                }
            }
        }
    };
    if cols.is_empty() && !has_wildcard_projection {
        return Err("authoring_ir: final SELECT has no output columns".to_string());
    }

    let mut seen = BTreeSet::new();
    let mut columns = Vec::new();
    for c in cols.into_iter() {
        let name = c.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let key = name.to_ascii_lowercase();
        if !seen.insert(key) {
            return Err(format!("authoring_ir: duplicate output column '{}'", name));
        }
        columns.push(ColumnIntent { name });
    }
    if columns.is_empty() && !has_wildcard_projection {
        return Err("authoring_ir: no usable output columns".to_string());
    }

    let mut tests = TestsIntent::default();
    for c in columns.iter() {
        let n = c.name.to_ascii_lowercase();
        if n == "id" || n.ends_with("_id") {
            tests.not_null.push(c.name.clone());
        }
    }
    tests.not_null.sort();
    tests.not_null.dedup();

    let mut out_notes = notes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    out_notes.truncate(40);

    Ok(ModelIntent {
        sql: normalized_sql.to_string(),
        notes: out_notes,
        columns,
        tests,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_sql_first_draft_extracts_columns_and_tests() {
        let ir = compile_sql_first_draft(
            "select\n  order_id,\n  amount\nfrom __SOURCE__\n",
            &["ok".to_string()],
        )
        .expect("ir");
        assert_eq!(ir.columns.len(), 2);
        let names = ir
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect::<BTreeSet<_>>();
        assert!(names.contains("order_id"));
        assert!(names.contains("amount"));
        assert_eq!(ir.tests.not_null, vec!["order_id".to_string()]);
    }
}
