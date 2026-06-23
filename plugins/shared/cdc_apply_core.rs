/// Replay-safe CDC apply logic for SQL-based exact-once sinks.
///
/// Sinks own their backend definitions by implementing `CdcApplyBackend`.
/// This module owns the shared SQL generation algorithm.
pub trait CdcApplyBackend {
    const ORDER_TOKEN_TYPE: &'static str;

    fn binary_literal(hex: &str) -> String;

    fn tx_begin() -> &'static str {
        "BEGIN;\n"
    }

    fn tx_commit() -> &'static str {
        "\nCOMMIT;"
    }

    fn merge_keyword() -> &'static str {
        "MERGE INTO"
    }

    fn ddl_add_order_token_column(fq_table: &str) -> String {
        format!(
            "ALTER TABLE {fq_table} ADD COLUMN IF NOT EXISTS \"_skippr_order_token\" {}",
            Self::ORDER_TOKEN_TYPE,
        )
    }

    fn ddl_create_tombstone_table(
        fq_tombstone_table: &str,
        business_key_cols: &[(String, String)],
    ) -> String {
        let mut col_defs: Vec<String> = business_key_cols
            .iter()
            .map(|(name, ty)| format!("\"{}\" {} NOT NULL", name, ty))
            .collect();
        col_defs.push(format!(
            "\"_skippr_order_token\" {} NOT NULL",
            Self::ORDER_TOKEN_TYPE
        ));

        let pk_cols: Vec<String> = business_key_cols
            .iter()
            .map(|(name, _)| format!("\"{}\"", name))
            .collect();

        format!(
            "CREATE TABLE IF NOT EXISTS {} ({}, PRIMARY KEY ({}))",
            fq_tombstone_table,
            col_defs.join(", "),
            pk_cols.join(", "),
        )
    }

    fn upsert_if_newer_sql(
        fq_table: &str,
        fq_tombstone_table: &str,
        all_col_names: &[String],
        all_col_values: &[String],
        business_key_names: &[String],
        order_token_hex: &str,
    ) -> String {
        let token = Self::binary_literal(order_token_hex);

        let tombstone_bk_match = tombstone_match_with_row_values(
            fq_tombstone_table,
            all_col_names,
            all_col_values,
            business_key_names,
        );

        let source_cols: Vec<String> = all_col_names
            .iter()
            .zip(all_col_values.iter())
            .map(|(name, val)| format!("{val} AS {name}"))
            .collect();
        let source_select = source_cols.join(", ");

        let on_clause: Vec<String> = business_key_names
            .iter()
            .map(|c| format!("t.{c} = s.{c}"))
            .collect();
        let on_match = on_clause.join(" AND ");

        let update_set: Vec<String> = all_col_names
            .iter()
            .filter(|c| !business_key_names.contains(c))
            .map(|c| format!("{c} = s.{c}"))
            .collect();
        let update_clause = update_set.join(", ");

        let insert_vals: Vec<String> = all_col_names.iter().map(|c| format!("s.{c}")).collect();
        let insert_val_list = insert_vals.join(", ");

        format!(
            "{}\
             {} {} AS t\n\
             USING (SELECT {}\n\
             WHERE NOT EXISTS (\n\
               SELECT 1 FROM {}\n\
               WHERE {}\n\
               AND {}.\"_skippr_order_token\" >= {}\n\
             )) AS s\n\
             ON {}\n\
             WHEN MATCHED AND (\n\
               t.\"_skippr_order_token\" IS NULL\n\
               OR t.\"_skippr_order_token\" < {}\n\
             ) THEN UPDATE SET {}\n\
             WHEN NOT MATCHED THEN INSERT ({})\n\
             VALUES ({});\n\
             DELETE FROM {}\n\
             WHERE {}\n\
             AND {}.\"_skippr_order_token\" < {};{}",
            Self::tx_begin(),
            Self::merge_keyword(),
            fq_table,
            source_select,
            fq_tombstone_table,
            tombstone_bk_match,
            fq_tombstone_table,
            token,
            on_match,
            token,
            update_clause,
            all_col_names.join(", "),
            insert_val_list,
            fq_tombstone_table,
            tombstone_bk_match,
            fq_tombstone_table,
            token,
            Self::tx_commit(),
        )
    }

    fn delete_if_newer_sql(
        fq_table: &str,
        fq_tombstone_table: &str,
        business_key_names: &[String],
        business_key_values: &[String],
        _business_key_types: &[String],
        order_token_hex: &str,
    ) -> String {
        let token = Self::binary_literal(order_token_hex);

        let delete_match = qualified_matches(business_key_names, business_key_values, None);
        let on_match = business_key_names
            .iter()
            .map(|c| format!("t.{c} = s.{c}"))
            .collect::<Vec<_>>()
            .join(" AND ");

        let source_cols: Vec<String> = business_key_names
            .iter()
            .zip(business_key_values.iter())
            .map(|(name, val)| format!("{val} AS {name}"))
            .chain(std::iter::once(format!(
                "{token} AS \"_skippr_order_token\""
            )))
            .collect();
        let source_select = source_cols.join(", ");

        let tombstone_cols: Vec<String> = business_key_names
            .iter()
            .cloned()
            .chain(std::iter::once("\"_skippr_order_token\"".to_string()))
            .collect();
        let insert_vals: Vec<String> = business_key_names
            .iter()
            .map(|c| format!("s.{c}"))
            .chain(std::iter::once("s.\"_skippr_order_token\"".to_string()))
            .collect();

        format!(
            "{}\
             DELETE FROM {}\n\
             WHERE {}\n\
             AND (\n\
               {}.\"_skippr_order_token\" IS NULL\n\
               OR {}.\"_skippr_order_token\" < {}\n\
             );\n\
             {} {} AS t\n\
             USING (SELECT {}) AS s\n\
             ON {}\n\
             WHEN MATCHED AND t.\"_skippr_order_token\" < s.\"_skippr_order_token\"\n\
               THEN UPDATE SET \"_skippr_order_token\" = s.\"_skippr_order_token\"\n\
             WHEN NOT MATCHED\n\
               THEN INSERT ({}) VALUES ({});{}",
            Self::tx_begin(),
            fq_table,
            delete_match,
            fq_table,
            fq_table,
            token,
            Self::merge_keyword(),
            fq_tombstone_table,
            source_select,
            on_match,
            tombstone_cols.join(", "),
            insert_vals.join(", "),
            Self::tx_commit(),
        )
    }
}

/// Compute the fully-qualified tombstone table name from a target table name.
/// Supports 1-part (`table`), 2-part (`schema.table`), and 3-part
/// (`catalog.schema.table`) identifiers. The split is on the last dot,
/// preserving the full schema/catalog prefix.
pub fn tombstone_table_name(fq_table: &str) -> String {
    if let Some((prefix, table)) = fq_table.rsplit_once('.') {
        let clean_table = table.trim_matches('"');
        format!("{}.\"_skippr_tombstones_{}\"", prefix, clean_table)
    } else {
        let clean = fq_table.trim_matches('"');
        format!("\"_skippr_tombstones_{}\"", clean)
    }
}

pub fn ddl_add_order_token_column<B: CdcApplyBackend>(fq_table: &str) -> String {
    B::ddl_add_order_token_column(fq_table)
}

pub fn ddl_create_tombstone_table<B: CdcApplyBackend>(
    fq_tombstone_table: &str,
    business_key_cols: &[(String, String)],
) -> String {
    B::ddl_create_tombstone_table(fq_tombstone_table, business_key_cols)
}

pub fn upsert_if_newer_sql<B: CdcApplyBackend>(
    fq_table: &str,
    fq_tombstone_table: &str,
    all_col_names: &[String],
    all_col_values: &[String],
    business_key_names: &[String],
    order_token_hex: &str,
) -> String {
    B::upsert_if_newer_sql(
        fq_table,
        fq_tombstone_table,
        all_col_names,
        all_col_values,
        business_key_names,
        order_token_hex,
    )
}

pub fn delete_if_newer_sql<B: CdcApplyBackend>(
    fq_table: &str,
    fq_tombstone_table: &str,
    business_key_names: &[String],
    business_key_values: &[String],
    business_key_types: &[String],
    order_token_hex: &str,
) -> String {
    B::delete_if_newer_sql(
        fq_table,
        fq_tombstone_table,
        business_key_names,
        business_key_values,
        business_key_types,
        order_token_hex,
    )
}

#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CdcBatchStatement {
    Upsert {
        all_col_names: Vec<String>,
        all_col_values: Vec<String>,
        business_key_names: Vec<String>,
        order_token_hex: String,
    },
    Delete {
        business_key_names: Vec<String>,
        business_key_values: Vec<String>,
        business_key_types: Vec<String>,
        order_token_hex: String,
    },
}

#[allow(dead_code)]
pub fn batched_cdc_apply_sql<B: CdcApplyBackend>(
    fq_table: &str,
    fq_tombstone_table: &str,
    statements: &[CdcBatchStatement],
) -> String {
    let mut body = String::new();
    for statement in statements {
        let sql = match statement {
            CdcBatchStatement::Upsert {
                all_col_names,
                all_col_values,
                business_key_names,
                order_token_hex,
            } => B::upsert_if_newer_sql(
                fq_table,
                fq_tombstone_table,
                all_col_names,
                all_col_values,
                business_key_names,
                order_token_hex,
            ),
            CdcBatchStatement::Delete {
                business_key_names,
                business_key_values,
                business_key_types,
                order_token_hex,
            } => B::delete_if_newer_sql(
                fq_table,
                fq_tombstone_table,
                business_key_names,
                business_key_values,
                business_key_types,
                order_token_hex,
            ),
        };
        let without_wrapping_tx = sql
            .trim()
            .strip_prefix(B::tx_begin().trim())
            .unwrap_or(sql.trim())
            .trim()
            .strip_suffix(B::tx_commit().trim())
            .unwrap_or(sql.trim())
            .trim();
        body.push_str(without_wrapping_tx);
        if !without_wrapping_tx.ends_with(';') {
            body.push(';');
        }
        body.push('\n');
    }
    format!("{}{}{}", B::tx_begin(), body, B::tx_commit())
}

fn find_business_key_value<'a>(
    all_col_names: &'a [String],
    all_col_values: &'a [String],
    business_key: &str,
) -> &'a str {
    let idx = all_col_names
        .iter()
        .position(|n| n == business_key)
        .unwrap_or(0);
    &all_col_values[idx]
}

fn tombstone_match_with_row_values(
    fq_tombstone_table: &str,
    all_col_names: &[String],
    all_col_values: &[String],
    business_key_names: &[String],
) -> String {
    business_key_names
        .iter()
        .map(|name| {
            let value = find_business_key_value(all_col_names, all_col_values, name);
            format!("{fq_tombstone_table}.{name} = {value}")
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn qualified_matches(names: &[String], values: &[String], qualifier: Option<&str>) -> String {
    names
        .iter()
        .zip(values.iter())
        .map(|(name, value)| match qualifier {
            Some(prefix) => format!("{prefix}.{name} = {value}"),
            None => format!("{name} = {value}"),
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::tombstone_table_name;

    #[test]
    fn tombstone_table_name_with_schema() {
        assert_eq!(
            tombstone_table_name("\"public\".\"users\""),
            "\"public\".\"_skippr_tombstones_users\""
        );
    }

    #[test]
    fn tombstone_table_name_without_schema() {
        assert_eq!(
            tombstone_table_name("users"),
            "\"_skippr_tombstones_users\""
        );
    }
}
