/// Replay-safe CDC apply logic for SQL-based exact-once sinks.
///
/// Generates dialect-specific SQL for:
/// - Adding `_skippr_order_token` to target tables
/// - Creating companion `_skippr_tombstones` tables
/// - Upsert-if-newer with stale-write protection
/// - Delete-if-newer with transactional tombstone writes

/// SQL dialect for statement generation.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    Postgres,
    Snowflake,
    BigQuery,
    Redshift,
    ClickHouse,
    Motherduck,
    Synapse,
    Databricks,
}

impl SqlDialect {
    fn order_token_type(self) -> &'static str {
        match self {
            Self::Postgres => "BYTEA",
            Self::Snowflake | Self::Databricks => "BINARY",
            Self::BigQuery => "BYTES",
            Self::Redshift => "VARBYTE",
            Self::ClickHouse => "String",
            Self::Motherduck => "BLOB",
            Self::Synapse => "VARBINARY(MAX)",
        }
    }

    fn binary_literal(self, hex: &str) -> String {
        match self {
            Self::Postgres => format!("decode('{hex}', 'hex')"),
            Self::Snowflake => format!("HEX_DECODE_BINARY('{hex}')"),
            Self::BigQuery | Self::Redshift => format!("FROM_HEX('{hex}')"),
            Self::ClickHouse => format!("'{hex}'"),
            Self::Motherduck => format!("'\\x{hex}'::BLOB"),
            Self::Synapse => format!("CONVERT(VARBINARY(MAX), 0x{hex})"),
            Self::Databricks => format!("X'{hex}'"),
        }
    }

    fn tx_begin(self) -> &'static str {
        match self {
            Self::ClickHouse => "",
            Self::BigQuery => "BEGIN TRANSACTION;\n",
            _ => "BEGIN;\n",
        }
    }

    fn tx_commit(self) -> &'static str {
        match self {
            Self::ClickHouse => "",
            Self::BigQuery => "\nCOMMIT TRANSACTION;",
            _ => "\nCOMMIT;",
        }
    }

    fn merge_keyword(self) -> &'static str {
        match self {
            Self::BigQuery => "MERGE",
            _ => "MERGE INTO",
        }
    }
}

/// Generate DDL to add the `_skippr_order_token` column to an existing table.
pub fn ddl_add_order_token_column(dialect: SqlDialect, fq_table: &str) -> String {
    let token_type = dialect.order_token_type();
    match dialect {
        SqlDialect::Synapse => {
            format!("ALTER TABLE {fq_table} ADD \"_skippr_order_token\" {token_type}",)
        }
        _ => format!(
            "ALTER TABLE {fq_table} ADD COLUMN IF NOT EXISTS \"_skippr_order_token\" {token_type}",
        ),
    }
}

/// Generate DDL for the companion tombstone table.
///
/// Schema: one column per business key (matching the target table types),
/// plus `_skippr_order_token` (dialect-specific binary type) `NOT NULL` and a
/// composite primary key over the business key columns.
pub fn ddl_create_tombstone_table(
    dialect: SqlDialect,
    fq_tombstone_table: &str,
    business_key_cols: &[(String, String)],
) -> String {
    let token_type = dialect.order_token_type();

    let mut col_defs: Vec<String> = business_key_cols
        .iter()
        .map(|(name, pg_type)| format!("\"{}\" {} NOT NULL", name, pg_type))
        .collect();
    col_defs.push(format!("\"_skippr_order_token\" {token_type} NOT NULL"));

    let pk_cols: Vec<String> = business_key_cols
        .iter()
        .map(|(name, _)| format!("\"{}\"", name))
        .collect();

    match dialect {
        SqlDialect::ClickHouse => format!(
            "CREATE TABLE IF NOT EXISTS {} ({}) ENGINE = MergeTree() ORDER BY ({})",
            fq_tombstone_table,
            col_defs.join(", "),
            pk_cols.join(", "),
        ),
        _ => format!(
            "CREATE TABLE IF NOT EXISTS {} ({}, PRIMARY KEY ({}))",
            fq_tombstone_table,
            col_defs.join(", "),
            pk_cols.join(", "),
        ),
    }
}

/// Compute the fully-qualified tombstone table name from a target table name.
/// Supports 1-part (`table`), 2-part (`schema.table`), and 3-part
/// (`catalog.schema.table`) identifiers.  The split is on the *last* dot,
/// preserving the full schema/catalog prefix.
pub fn tombstone_table_name(fq_table: &str) -> String {
    if let Some((prefix, table)) = fq_table.rsplit_once('.') {
        let clean_table = table.trim_matches('"');
        format!("{}.\"_skippr_tombstones_{}\"", prefix, clean_table,)
    } else {
        let clean = fq_table.trim_matches('"');
        format!("\"_skippr_tombstones_{}\"", clean)
    }
}

/// Generate a transactional upsert-if-newer SQL block.
///
/// The statement:
/// 1. Checks the tombstone table for a newer delete
/// 2. Inserts or updates the target row only if the incoming token is newer
/// 3. Removes any stale tombstone for this key if the upsert wins
///
/// Returns a single SQL string wrapped in BEGIN/COMMIT (or dialect equivalent).
pub fn upsert_if_newer_sql(
    dialect: SqlDialect,
    fq_table: &str,
    fq_tombstone_table: &str,
    all_col_names: &[String],
    all_col_values: &[String],
    business_key_names: &[String],
    order_token_hex: &str,
) -> String {
    let token = dialect.binary_literal(order_token_hex);

    let tombstone_bk_literal: Vec<String> = business_key_names
        .iter()
        .map(|c| {
            let idx = all_col_names.iter().position(|n| n == c).unwrap_or(0);
            format!("{fq_tombstone_table}.{c} = {}", all_col_values[idx],)
        })
        .collect();
    let tombstone_bk_match = tombstone_bk_literal.join(" AND ");
    let tombstone_bk_unqualified: Vec<String> = business_key_names
        .iter()
        .map(|c| {
            let idx = all_col_names.iter().position(|n| n == c).unwrap_or(0);
            format!("{c} = {}", all_col_values[idx])
        })
        .collect();
    let tombstone_bk_match_unqualified = tombstone_bk_unqualified.join(" AND ");

    let target_bk_literal: Vec<String> = business_key_names
        .iter()
        .map(|c| {
            let idx = all_col_names.iter().position(|n| n == c).unwrap_or(0);
            format!("{c} = {}", all_col_values[idx])
        })
        .collect();
    let target_bk_match = target_bk_literal.join(" AND ");

    match dialect {
        SqlDialect::Postgres => {
            let col_list = all_col_names.join(", ");
            let val_list = all_col_values.join(", ");

            let conflict_cols: String = business_key_names.join(", ");

            let update_set: Vec<String> = all_col_names
                .iter()
                .filter(|c| {
                    !business_key_names.contains(c) && c.as_str() != "\"_skippr_order_token\""
                })
                .map(|c| format!("{c} = EXCLUDED.{c}"))
                .collect();
            let update_set_with_token = {
                let mut parts = update_set;
                parts
                    .push("\"_skippr_order_token\" = EXCLUDED.\"_skippr_order_token\"".to_string());
                parts.join(", ")
            };

            let bk_where: Vec<String> = business_key_names
                .iter()
                .map(|c| format!("{fq_tombstone_table}.{c} = EXCLUDED.{c}"))
                .collect();
            let bk_match = bk_where.join(" AND ");

            format!(
                "BEGIN;\n\
                 INSERT INTO {fq_table} ({col_list})\n\
                 SELECT {val_list}\n\
                 WHERE NOT EXISTS (\n\
                   SELECT 1 FROM {fq_tombstone_table}\n\
                   WHERE {tombstone_bk_match}\n\
                   AND {fq_tombstone_table}.\"_skippr_order_token\" >= {token}\n\
                 )\n\
                 ON CONFLICT ({conflict_cols}) DO UPDATE SET {update_set_with_token}\n\
                 WHERE (\n\
                   {fq_table}.\"_skippr_order_token\" IS NULL\n\
                   OR {fq_table}.\"_skippr_order_token\" < {token}\n\
                 )\n\
                 AND NOT EXISTS (\n\
                   SELECT 1 FROM {fq_tombstone_table}\n\
                   WHERE {bk_match}\n\
                   AND {fq_tombstone_table}.\"_skippr_order_token\" >= {token}\n\
                 );\n\
                 DELETE FROM {fq_tombstone_table}\n\
                 WHERE {tombstone_bk_match}\n\
                 AND {fq_tombstone_table}.\"_skippr_order_token\" < {token};\n\
                 COMMIT;"
            )
        }
        SqlDialect::ClickHouse => {
            let col_list = all_col_names.join(", ");
            let val_list = all_col_values.join(", ");

            let update_assignments: Vec<String> = all_col_names
                .iter()
                .zip(all_col_values.iter())
                .filter(|(name, _)| !business_key_names.contains(name))
                .map(|(name, val)| format!("{name} = {val}"))
                .collect();

            format!(
                "INSERT INTO {fq_table} ({col_list})\n\
                 SELECT {val_list}\n\
                 WHERE NOT EXISTS (SELECT 1 FROM {fq_table} WHERE {target_bk_match})\n\
                 AND NOT EXISTS (\n\
                   SELECT 1 FROM {fq_tombstone_table}\n\
                   WHERE {tombstone_bk_match}\n\
                   AND {fq_tombstone_table}.\"_skippr_order_token\" >= {token}\n\
                 );\n\
                 ALTER TABLE {fq_table} UPDATE {update_set}\n\
                 WHERE {target_bk_match}\n\
                 AND (\"_skippr_order_token\" IS NULL OR \"_skippr_order_token\" < {token})\n\
                 AND NOT EXISTS (\n\
                   SELECT 1 FROM {fq_tombstone_table}\n\
                   WHERE {tombstone_bk_match}\n\
                   AND {fq_tombstone_table}.\"_skippr_order_token\" >= {token}\n\
                 );\n\
                 ALTER TABLE {fq_tombstone_table} DELETE\n\
                 WHERE {tombstone_bk_match_unqualified}\n\
                 AND \"_skippr_order_token\" < {token};",
                update_set = update_assignments.join(", "),
            )
        }
        // MERGE-based dialects: Snowflake, BigQuery, Redshift, Motherduck, Synapse, Databricks
        _ => {
            let col_list = all_col_names.join(", ");
            let merge_kw = dialect.merge_keyword();
            let begin = dialect.tx_begin();
            let commit = dialect.tx_commit();

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
                "{begin}\
                 {merge_kw} {fq_table} AS t\n\
                 USING (SELECT {source_select}\n\
                 WHERE NOT EXISTS (\n\
                   SELECT 1 FROM {fq_tombstone_table}\n\
                   WHERE {tombstone_bk_match}\n\
                   AND {fq_tombstone_table}.\"_skippr_order_token\" >= {token}\n\
                 )) AS s\n\
                 ON {on_match}\n\
                 WHEN MATCHED AND (\n\
                   t.\"_skippr_order_token\" IS NULL\n\
                   OR t.\"_skippr_order_token\" < {token}\n\
                 ) THEN UPDATE SET {update_clause}\n\
                 WHEN NOT MATCHED THEN INSERT ({col_list})\n\
                 VALUES ({insert_val_list});\n\
                 DELETE FROM {fq_tombstone_table}\n\
                 WHERE {tombstone_bk_match}\n\
                 AND {fq_tombstone_table}.\"_skippr_order_token\" < {token};{commit}"
            )
        }
    }
}

/// Generate a transactional delete-if-newer SQL block.
///
/// The statement:
/// 1. Deletes the target row only if the incoming token is newer
/// 2. Upserts the tombstone row with the delete token
///
/// Returns a single SQL string wrapped in BEGIN/COMMIT (or dialect equivalent).
pub fn delete_if_newer_sql(
    dialect: SqlDialect,
    fq_table: &str,
    fq_tombstone_table: &str,
    business_key_names: &[String],
    business_key_values: &[String],
    _business_key_types: &[String],
    order_token_hex: &str,
) -> String {
    let token = dialect.binary_literal(order_token_hex);

    let delete_where: Vec<String> = business_key_names
        .iter()
        .zip(business_key_values.iter())
        .map(|(name, val)| format!("{name} = {val}"))
        .collect();
    let delete_match = delete_where.join(" AND ");

    let tombstone_bk_literal: Vec<String> = business_key_names
        .iter()
        .zip(business_key_values.iter())
        .map(|(name, val)| format!("{fq_tombstone_table}.{name} = {val}"))
        .collect();
    let tombstone_bk_match = tombstone_bk_literal.join(" AND ");

    match dialect {
        SqlDialect::Postgres => {
            let tombstone_cols: Vec<String> = business_key_names
                .iter()
                .chain(std::iter::once(&"\"_skippr_order_token\"".to_string()))
                .cloned()
                .collect();
            let tombstone_vals: Vec<String> = business_key_values
                .iter()
                .cloned()
                .chain(std::iter::once(token.clone()))
                .collect();

            let conflict_cols = business_key_names.join(", ");

            format!(
                "BEGIN;\n\
                 DELETE FROM {fq_table}\n\
                 WHERE {delete_match}\n\
                 AND (\n\
                   {fq_table}.\"_skippr_order_token\" IS NULL\n\
                   OR {fq_table}.\"_skippr_order_token\" < {token}\n\
                 );\n\
                 INSERT INTO {fq_tombstone_table} ({tombstone_col_list})\n\
                 VALUES ({tombstone_val_list})\n\
                 ON CONFLICT ({conflict_cols}) DO UPDATE\n\
                 SET \"_skippr_order_token\" = EXCLUDED.\"_skippr_order_token\"\n\
                 WHERE {fq_tombstone_table}.\"_skippr_order_token\" < EXCLUDED.\"_skippr_order_token\";\n\
                 COMMIT;",
                tombstone_col_list = tombstone_cols.join(", "),
                tombstone_val_list = tombstone_vals.join(", "),
            )
        }
        SqlDialect::ClickHouse => {
            let tombstone_cols: Vec<String> = business_key_names
                .iter()
                .chain(std::iter::once(&"\"_skippr_order_token\"".to_string()))
                .cloned()
                .collect();
            let tombstone_vals: Vec<String> = business_key_values
                .iter()
                .cloned()
                .chain(std::iter::once(token.clone()))
                .collect();

            format!(
                "ALTER TABLE {fq_table} DELETE\n\
                 WHERE {delete_match}\n\
                 AND (\"_skippr_order_token\" IS NULL OR \"_skippr_order_token\" < {token});\n\
                 INSERT INTO {fq_tombstone_table} ({tombstone_col_list})\n\
                 SELECT {tombstone_val_list}\n\
                 WHERE NOT EXISTS (\n\
                   SELECT 1 FROM {fq_tombstone_table}\n\
                   WHERE {tombstone_bk_match}\n\
                   AND {fq_tombstone_table}.\"_skippr_order_token\" >= {token}\n\
                 );",
                tombstone_col_list = tombstone_cols.join(", "),
                tombstone_val_list = tombstone_vals.join(", "),
            )
        }
        // MERGE-based dialects: Snowflake, BigQuery, Redshift, Motherduck, Synapse, Databricks
        _ => {
            let merge_kw = dialect.merge_keyword();
            let begin = dialect.tx_begin();
            let commit = dialect.tx_commit();

            let source_cols: Vec<String> = business_key_names
                .iter()
                .zip(business_key_values.iter())
                .map(|(name, val)| format!("{val} AS {name}"))
                .chain(std::iter::once(format!(
                    "{token} AS \"_skippr_order_token\""
                )))
                .collect();
            let source_select = source_cols.join(", ");

            let on_clause: Vec<String> = business_key_names
                .iter()
                .map(|c| format!("t.{c} = s.{c}"))
                .collect();
            let on_match = on_clause.join(" AND ");

            let tombstone_col_list: Vec<String> = business_key_names
                .iter()
                .chain(std::iter::once(&"\"_skippr_order_token\"".to_string()))
                .cloned()
                .collect();
            let insert_vals: Vec<String> = business_key_names
                .iter()
                .map(|c| format!("s.{c}"))
                .chain(std::iter::once("s.\"_skippr_order_token\"".to_string()))
                .collect();

            format!(
                "{begin}\
                 DELETE FROM {fq_table}\n\
                 WHERE {delete_match}\n\
                 AND (\n\
                   {fq_table}.\"_skippr_order_token\" IS NULL\n\
                   OR {fq_table}.\"_skippr_order_token\" < {token}\n\
                 );\n\
                 {merge_kw} {fq_tombstone_table} AS t\n\
                 USING (SELECT {source_select}) AS s\n\
                 ON {on_match}\n\
                 WHEN MATCHED AND t.\"_skippr_order_token\" < s.\"_skippr_order_token\"\n\
                   THEN UPDATE SET \"_skippr_order_token\" = s.\"_skippr_order_token\"\n\
                 WHEN NOT MATCHED\n\
                   THEN INSERT ({tombstone_cols}) VALUES ({insert_val_list});{commit}",
                tombstone_cols = tombstone_col_list.join(", "),
                insert_val_list = insert_vals.join(", "),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tombstone_table_name_with_schema() {
        assert_eq!(
            tombstone_table_name("\"public\".\"users\""),
            "\"public\".\"_skippr_tombstones_users\""
        );
    }

    #[test]
    fn test_tombstone_table_name_without_schema() {
        assert_eq!(
            tombstone_table_name("users"),
            "\"_skippr_tombstones_users\""
        );
    }

    #[test]
    fn test_ddl_add_order_token_column() {
        let sql = ddl_add_order_token_column(SqlDialect::Postgres, "\"public\".\"users\"");
        assert!(sql.contains("_skippr_order_token"));
        assert!(sql.contains("BYTEA"));
        assert!(sql.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_ddl_create_tombstone_table() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::Postgres,
            "\"public\".\"_skippr_tombstones_users\"",
            &[("id".to_string(), "BIGINT".to_string())],
        );
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS"));
        assert!(sql.contains("\"id\" BIGINT NOT NULL"));
        assert!(sql.contains("\"_skippr_order_token\" BYTEA NOT NULL"));
        assert!(sql.contains("PRIMARY KEY (\"id\")"));
    }

    #[test]
    fn test_ddl_create_tombstone_table_composite_key() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::Postgres,
            "\"public\".\"_skippr_tombstones_orders\"",
            &[
                ("org_id".to_string(), "BIGINT".to_string()),
                ("order_id".to_string(), "BIGINT".to_string()),
            ],
        );
        assert!(sql.contains("PRIMARY KEY (\"org_id\", \"order_id\")"));
    }

    #[test]
    fn test_upsert_if_newer_sql_structure() {
        let sql = upsert_if_newer_sql(
            SqlDialect::Postgres,
            "\"public\".\"users\"",
            "\"public\".\"_skippr_tombstones_users\"",
            &[
                "\"id\"".to_string(),
                "\"name\"".to_string(),
                "\"_skippr_order_token\"".to_string(),
            ],
            &[
                "42".to_string(),
                "'Alice'".to_string(),
                "decode('00000001', 'hex')".to_string(),
            ],
            &["\"id\"".to_string()],
            "00000001",
        );

        assert!(sql.contains("BEGIN;"));
        assert!(sql.contains("COMMIT;"));
        assert!(sql.contains("INSERT INTO \"public\".\"users\""));
        assert!(sql.contains("ON CONFLICT (\"id\") DO UPDATE"));
        assert!(sql.contains("_skippr_order_token"));
        assert!(sql.contains("NOT EXISTS"));
        assert!(sql.contains("DELETE FROM \"public\".\"_skippr_tombstones_users\""));
    }

    #[test]
    fn test_delete_if_newer_sql_structure() {
        let sql = delete_if_newer_sql(
            SqlDialect::Postgres,
            "\"public\".\"users\"",
            "\"public\".\"_skippr_tombstones_users\"",
            &["\"id\"".to_string()],
            &["42".to_string()],
            &["BIGINT".to_string()],
            "00000002",
        );

        assert!(sql.contains("BEGIN;"));
        assert!(sql.contains("COMMIT;"));
        assert!(sql.contains("DELETE FROM \"public\".\"users\""));
        assert!(sql.contains("INSERT INTO \"public\".\"_skippr_tombstones_users\""));
        assert!(sql.contains("ON CONFLICT (\"id\") DO UPDATE"));
        assert!(sql.contains("00000002"));
    }

    // --- Snowflake ---

    #[test]
    fn test_snowflake_ddl_add_order_token_column() {
        let sql = ddl_add_order_token_column(SqlDialect::Snowflake, "\"db\".\"users\"");
        assert!(sql.contains("BINARY"));
        assert!(sql.contains("IF NOT EXISTS"));
        assert!(!sql.contains("BYTEA"));
    }

    #[test]
    fn test_snowflake_ddl_create_tombstone_table() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::Snowflake,
            "\"db\".\"_skippr_tombstones_users\"",
            &[("id".to_string(), "NUMBER".to_string())],
        );
        assert!(sql.contains("\"_skippr_order_token\" BINARY NOT NULL"));
        assert!(sql.contains("PRIMARY KEY (\"id\")"));
    }

    #[test]
    fn test_snowflake_upsert_if_newer_sql() {
        let sql = upsert_if_newer_sql(
            SqlDialect::Snowflake,
            "\"db\".\"users\"",
            "\"db\".\"_skippr_tombstones_users\"",
            &[
                "\"id\"".to_string(),
                "\"name\"".to_string(),
                "\"_skippr_order_token\"".to_string(),
            ],
            &[
                "42".to_string(),
                "'Alice'".to_string(),
                "HEX_DECODE_BINARY('00000001')".to_string(),
            ],
            &["\"id\"".to_string()],
            "00000001",
        );

        assert!(sql.contains("BEGIN;"));
        assert!(sql.contains("COMMIT;"));
        assert!(sql.contains("MERGE INTO \"db\".\"users\" AS t"));
        assert!(sql.contains("USING (SELECT"));
        assert!(sql.contains("HEX_DECODE_BINARY('00000001')"));
        assert!(sql.contains("WHEN MATCHED"));
        assert!(sql.contains("WHEN NOT MATCHED"));
        assert!(sql.contains("DELETE FROM \"db\".\"_skippr_tombstones_users\""));
    }

    #[test]
    fn test_snowflake_delete_if_newer_sql() {
        let sql = delete_if_newer_sql(
            SqlDialect::Snowflake,
            "\"db\".\"users\"",
            "\"db\".\"_skippr_tombstones_users\"",
            &["\"id\"".to_string()],
            &["42".to_string()],
            &["NUMBER".to_string()],
            "00000002",
        );

        assert!(sql.contains("BEGIN;"));
        assert!(sql.contains("COMMIT;"));
        assert!(sql.contains("DELETE FROM \"db\".\"users\""));
        assert!(sql.contains("MERGE INTO \"db\".\"_skippr_tombstones_users\" AS t"));
        assert!(sql.contains("HEX_DECODE_BINARY('00000002')"));
    }

    // --- BigQuery ---

    #[test]
    fn test_bigquery_ddl_add_order_token_column() {
        let sql = ddl_add_order_token_column(SqlDialect::BigQuery, "project.dataset.users");
        assert!(sql.contains("BYTES"));
        assert!(sql.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_bigquery_ddl_create_tombstone_table() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::BigQuery,
            "project.dataset._skippr_tombstones_users",
            &[("id".to_string(), "INT64".to_string())],
        );
        assert!(sql.contains("\"_skippr_order_token\" BYTES NOT NULL"));
        assert!(sql.contains("PRIMARY KEY (\"id\")"));
    }

    #[test]
    fn test_bigquery_upsert_if_newer_sql() {
        let sql = upsert_if_newer_sql(
            SqlDialect::BigQuery,
            "project.dataset.users",
            "project.dataset._skippr_tombstones_users",
            &[
                "\"id\"".to_string(),
                "\"name\"".to_string(),
                "\"_skippr_order_token\"".to_string(),
            ],
            &[
                "42".to_string(),
                "'Alice'".to_string(),
                "FROM_HEX('00000001')".to_string(),
            ],
            &["\"id\"".to_string()],
            "00000001",
        );

        assert!(sql.contains("BEGIN TRANSACTION;"));
        assert!(sql.contains("COMMIT TRANSACTION;"));
        assert!(sql.contains("MERGE project.dataset.users AS t"));
        assert!(!sql.contains("MERGE INTO project"));
        assert!(sql.contains("FROM_HEX('00000001')"));
        assert!(sql.contains("WHEN MATCHED"));
        assert!(sql.contains("WHEN NOT MATCHED"));
    }

    #[test]
    fn test_bigquery_delete_if_newer_sql() {
        let sql = delete_if_newer_sql(
            SqlDialect::BigQuery,
            "project.dataset.users",
            "project.dataset._skippr_tombstones_users",
            &["\"id\"".to_string()],
            &["42".to_string()],
            &["INT64".to_string()],
            "00000002",
        );

        assert!(sql.contains("BEGIN TRANSACTION;"));
        assert!(sql.contains("COMMIT TRANSACTION;"));
        assert!(sql.contains("MERGE project.dataset._skippr_tombstones_users AS t"));
        assert!(!sql.contains("MERGE INTO project"));
        assert!(sql.contains("FROM_HEX('00000002')"));
    }

    // --- ClickHouse ---

    #[test]
    fn test_clickhouse_ddl_add_order_token_column() {
        let sql = ddl_add_order_token_column(SqlDialect::ClickHouse, "\"users\"");
        assert!(sql.contains("String"));
        assert!(!sql.contains("BYTEA"));
    }

    #[test]
    fn test_clickhouse_ddl_create_tombstone_table() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::ClickHouse,
            "\"_skippr_tombstones_users\"",
            &[("id".to_string(), "UInt64".to_string())],
        );
        assert!(sql.contains("ENGINE = MergeTree()"));
        assert!(sql.contains("ORDER BY (\"id\")"));
        assert!(!sql.contains("PRIMARY KEY"));
    }

    #[test]
    fn test_clickhouse_upsert_if_newer_no_transactions() {
        let sql = upsert_if_newer_sql(
            SqlDialect::ClickHouse,
            "\"users\"",
            "\"_skippr_tombstones_users\"",
            &[
                "\"id\"".to_string(),
                "\"name\"".to_string(),
                "\"_skippr_order_token\"".to_string(),
            ],
            &[
                "42".to_string(),
                "'Alice'".to_string(),
                "'00000001'".to_string(),
            ],
            &["\"id\"".to_string()],
            "00000001",
        );

        assert!(!sql.contains("BEGIN"));
        assert!(!sql.contains("COMMIT"));
        assert!(sql.contains("INSERT INTO \"users\""));
        assert!(sql.contains("ALTER TABLE \"users\" UPDATE"));
        assert!(sql.contains("ALTER TABLE \"_skippr_tombstones_users\" DELETE"));
    }

    #[test]
    fn test_clickhouse_delete_if_newer_no_transactions() {
        let sql = delete_if_newer_sql(
            SqlDialect::ClickHouse,
            "\"users\"",
            "\"_skippr_tombstones_users\"",
            &["\"id\"".to_string()],
            &["42".to_string()],
            &["UInt64".to_string()],
            "00000002",
        );

        assert!(!sql.contains("BEGIN"));
        assert!(!sql.contains("COMMIT"));
        assert!(sql.contains("ALTER TABLE \"users\" DELETE"));
        assert!(sql.contains("INSERT INTO \"_skippr_tombstones_users\""));
        assert!(sql.contains("'00000002'"));
    }

    // --- Other dialects: quick smoke tests ---

    #[test]
    fn test_redshift_binary_literal_in_upsert() {
        let sql = upsert_if_newer_sql(
            SqlDialect::Redshift,
            "\"public\".\"users\"",
            "\"public\".\"_skippr_tombstones_users\"",
            &["\"id\"".to_string(), "\"_skippr_order_token\"".to_string()],
            &["1".to_string(), "FROM_HEX('ab')".to_string()],
            &["\"id\"".to_string()],
            "ab",
        );
        assert!(sql.contains("FROM_HEX('ab')"));
        assert!(sql.contains("MERGE INTO"));
    }

    #[test]
    fn test_motherduck_binary_literal_in_upsert() {
        let sql = upsert_if_newer_sql(
            SqlDialect::Motherduck,
            "\"users\"",
            "\"_skippr_tombstones_users\"",
            &["\"id\"".to_string(), "\"_skippr_order_token\"".to_string()],
            &["1".to_string(), "'\\xab'::BLOB".to_string()],
            &["\"id\"".to_string()],
            "ab",
        );
        assert!(sql.contains("'\\xab'::BLOB"));
        assert!(sql.contains("MERGE INTO"));
    }

    #[test]
    fn test_synapse_ddl_no_if_not_exists() {
        let sql = ddl_add_order_token_column(SqlDialect::Synapse, "dbo.users");
        assert!(sql.contains("VARBINARY(MAX)"));
        assert!(!sql.contains("IF NOT EXISTS"));
    }

    #[test]
    fn test_databricks_binary_literal_in_upsert() {
        let sql = upsert_if_newer_sql(
            SqlDialect::Databricks,
            "\"catalog\".\"schema\".\"users\"",
            "\"catalog\".\"schema\".\"_skippr_tombstones_users\"",
            &["\"id\"".to_string(), "\"_skippr_order_token\"".to_string()],
            &["1".to_string(), "X'ab'".to_string()],
            &["\"id\"".to_string()],
            "ab",
        );
        assert!(sql.contains("X'ab'"));
        assert!(sql.contains("MERGE INTO"));
    }

    // =======================================================================
    // Per-dialect upsert/delete roundtrip tests
    // =======================================================================

    struct DialectFixture {
        dialect: SqlDialect,
        fq_table: &'static str,
        fq_tombstone: &'static str,
        binary_lit_fragment: &'static str,
    }

    fn dialect_fixtures() -> Vec<DialectFixture> {
        vec![
            DialectFixture {
                dialect: SqlDialect::Snowflake,
                fq_table: "\"db\".\"users\"",
                fq_tombstone: "\"db\".\"_skippr_tombstones_users\"",
                binary_lit_fragment: "HEX_DECODE_BINARY('",
            },
            DialectFixture {
                dialect: SqlDialect::BigQuery,
                fq_table: "project.dataset.users",
                fq_tombstone: "project.dataset._skippr_tombstones_users",
                binary_lit_fragment: "FROM_HEX('",
            },
            DialectFixture {
                dialect: SqlDialect::Redshift,
                fq_table: "\"public\".\"users\"",
                fq_tombstone: "\"public\".\"_skippr_tombstones_users\"",
                binary_lit_fragment: "FROM_HEX('",
            },
            DialectFixture {
                dialect: SqlDialect::ClickHouse,
                fq_table: "\"users\"",
                fq_tombstone: "\"_skippr_tombstones_users\"",
                binary_lit_fragment: "'00000001'",
            },
            DialectFixture {
                dialect: SqlDialect::Motherduck,
                fq_table: "\"users\"",
                fq_tombstone: "\"_skippr_tombstones_users\"",
                binary_lit_fragment: "'\\x00000001'::BLOB",
            },
            DialectFixture {
                dialect: SqlDialect::Synapse,
                fq_table: "dbo.users",
                fq_tombstone: "dbo._skippr_tombstones_users",
                binary_lit_fragment: "CONVERT(VARBINARY(MAX), 0x",
            },
            DialectFixture {
                dialect: SqlDialect::Databricks,
                fq_table: "\"catalog\".\"schema\".\"users\"",
                fq_tombstone: "\"catalog\".\"schema\".\"_skippr_tombstones_users\"",
                binary_lit_fragment: "X'00000001'",
            },
        ]
    }

    fn gen_upsert_for_fixture(f: &DialectFixture) -> String {
        let token_val = f.dialect.binary_literal("00000001");
        upsert_if_newer_sql(
            f.dialect,
            f.fq_table,
            f.fq_tombstone,
            &[
                "\"id\"".to_string(),
                "\"name\"".to_string(),
                "\"_skippr_order_token\"".to_string(),
            ],
            &["42".to_string(), "'Alice'".to_string(), token_val],
            &["\"id\"".to_string()],
            "00000001",
        )
    }

    fn gen_delete_for_fixture(f: &DialectFixture) -> String {
        delete_if_newer_sql(
            f.dialect,
            f.fq_table,
            f.fq_tombstone,
            &["\"id\"".to_string()],
            &["42".to_string()],
            &["BIGINT".to_string()],
            "00000001",
        )
    }

    #[test]
    fn test_snowflake_upsert_delete_roundtrip() {
        let f = &dialect_fixtures()[0];
        assert_eq!(f.dialect, SqlDialect::Snowflake);
        let upsert = gen_upsert_for_fixture(f);
        let delete = gen_delete_for_fixture(f);

        assert!(upsert.contains(f.binary_lit_fragment));
        assert!(upsert.contains("MERGE INTO"));
        assert!(upsert.contains("BEGIN;"));
        assert!(upsert.contains("COMMIT;"));
        assert!(delete.contains(f.fq_tombstone));
        assert!(delete.contains("BEGIN;"));
        assert!(delete.contains("COMMIT;"));
    }

    #[test]
    fn test_bigquery_upsert_delete_roundtrip() {
        let f = &dialect_fixtures()[1];
        assert_eq!(f.dialect, SqlDialect::BigQuery);
        let upsert = gen_upsert_for_fixture(f);
        let delete = gen_delete_for_fixture(f);

        assert!(upsert.contains(f.binary_lit_fragment));
        assert!(upsert.contains("MERGE "));
        assert!(!upsert.contains("MERGE INTO"));
        assert!(upsert.contains("BEGIN TRANSACTION;"));
        assert!(upsert.contains("COMMIT TRANSACTION;"));
        assert!(delete.contains(f.fq_tombstone));
        assert!(delete.contains("BEGIN TRANSACTION;"));
        assert!(delete.contains("COMMIT TRANSACTION;"));
    }

    #[test]
    fn test_redshift_upsert_delete_roundtrip() {
        let f = &dialect_fixtures()[2];
        assert_eq!(f.dialect, SqlDialect::Redshift);
        let upsert = gen_upsert_for_fixture(f);
        let delete = gen_delete_for_fixture(f);

        assert!(upsert.contains(f.binary_lit_fragment));
        assert!(upsert.contains("MERGE INTO"));
        assert!(upsert.contains("BEGIN;"));
        assert!(upsert.contains("COMMIT;"));
        assert!(delete.contains(f.fq_tombstone));
        assert!(delete.contains("BEGIN;"));
    }

    #[test]
    fn test_clickhouse_upsert_delete_roundtrip() {
        let f = &dialect_fixtures()[3];
        assert_eq!(f.dialect, SqlDialect::ClickHouse);
        let upsert = gen_upsert_for_fixture(f);
        let delete = gen_delete_for_fixture(f);

        assert!(upsert.contains(f.binary_lit_fragment));
        assert!(!upsert.contains("MERGE"));
        assert!(!upsert.contains("BEGIN"));
        assert!(!upsert.contains("COMMIT"));
        assert!(delete.contains(f.fq_tombstone));
        assert!(!delete.contains("BEGIN"));
        assert!(!delete.contains("COMMIT"));
    }

    #[test]
    fn test_motherduck_upsert_delete_roundtrip() {
        let f = &dialect_fixtures()[4];
        assert_eq!(f.dialect, SqlDialect::Motherduck);
        let upsert = gen_upsert_for_fixture(f);
        let delete = gen_delete_for_fixture(f);

        assert!(upsert.contains("::BLOB"));
        assert!(upsert.contains("MERGE INTO"));
        assert!(upsert.contains("BEGIN;"));
        assert!(upsert.contains("COMMIT;"));
        assert!(delete.contains(f.fq_tombstone));
        assert!(delete.contains("BEGIN;"));
    }

    #[test]
    fn test_synapse_upsert_delete_roundtrip() {
        let f = &dialect_fixtures()[5];
        assert_eq!(f.dialect, SqlDialect::Synapse);
        let upsert = gen_upsert_for_fixture(f);
        let delete = gen_delete_for_fixture(f);

        assert!(upsert.contains(f.binary_lit_fragment));
        assert!(upsert.contains("MERGE INTO"));
        assert!(upsert.contains("BEGIN;"));
        assert!(upsert.contains("COMMIT;"));
        assert!(delete.contains(f.fq_tombstone));
        assert!(delete.contains("BEGIN;"));
    }

    #[test]
    fn test_databricks_upsert_delete_roundtrip() {
        let f = &dialect_fixtures()[6];
        assert_eq!(f.dialect, SqlDialect::Databricks);
        let upsert = gen_upsert_for_fixture(f);
        let delete = gen_delete_for_fixture(f);

        assert!(upsert.contains("X'00000001'"));
        assert!(upsert.contains("MERGE INTO"));
        assert!(upsert.contains("BEGIN;"));
        assert!(upsert.contains("COMMIT;"));
        assert!(delete.contains(f.fq_tombstone));
        assert!(delete.contains("BEGIN;"));
    }

    // =======================================================================
    // Tombstone table DDL per dialect
    // =======================================================================

    #[test]
    fn test_snowflake_tombstone_table_ddl() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::Snowflake,
            "\"db\".\"_skippr_tombstones_users\"",
            &[("id".to_string(), "NUMBER".to_string())],
        );
        assert!(sql.contains("PRIMARY KEY"));
        assert!(sql.contains("BINARY NOT NULL"));
        assert!(!sql.contains("MergeTree"));
    }

    #[test]
    fn test_bigquery_tombstone_table_ddl() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::BigQuery,
            "project.dataset._skippr_tombstones_users",
            &[("id".to_string(), "INT64".to_string())],
        );
        assert!(sql.contains("PRIMARY KEY"));
        assert!(sql.contains("BYTES NOT NULL"));
        assert!(!sql.contains("MergeTree"));
    }

    #[test]
    fn test_redshift_tombstone_table_ddl() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::Redshift,
            "\"public\".\"_skippr_tombstones_users\"",
            &[("id".to_string(), "BIGINT".to_string())],
        );
        assert!(sql.contains("PRIMARY KEY"));
        assert!(sql.contains("VARBYTE NOT NULL"));
        assert!(!sql.contains("MergeTree"));
    }

    #[test]
    fn test_clickhouse_tombstone_table_ddl() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::ClickHouse,
            "\"_skippr_tombstones_users\"",
            &[("id".to_string(), "UInt64".to_string())],
        );
        assert!(sql.contains("ENGINE = MergeTree()"));
        assert!(sql.contains("ORDER BY (\"id\")"));
        assert!(!sql.contains("PRIMARY KEY"));
        assert!(sql.contains("String NOT NULL"));
    }

    #[test]
    fn test_motherduck_tombstone_table_ddl() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::Motherduck,
            "\"_skippr_tombstones_users\"",
            &[("id".to_string(), "BIGINT".to_string())],
        );
        assert!(sql.contains("PRIMARY KEY"));
        assert!(sql.contains("BLOB NOT NULL"));
        assert!(!sql.contains("MergeTree"));
    }

    #[test]
    fn test_synapse_tombstone_table_ddl() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::Synapse,
            "dbo._skippr_tombstones_users",
            &[("id".to_string(), "BIGINT".to_string())],
        );
        assert!(sql.contains("PRIMARY KEY"));
        assert!(sql.contains("VARBINARY(MAX) NOT NULL"));
        assert!(!sql.contains("MergeTree"));
    }

    #[test]
    fn test_databricks_tombstone_table_ddl() {
        let sql = ddl_create_tombstone_table(
            SqlDialect::Databricks,
            "\"catalog\".\"schema\".\"_skippr_tombstones_users\"",
            &[("id".to_string(), "BIGINT".to_string())],
        );
        assert!(sql.contains("PRIMARY KEY"));
        assert!(sql.contains("BINARY NOT NULL"));
        assert!(!sql.contains("MergeTree"));
    }

    // =======================================================================
    // Composite key consistency across all dialects
    // =======================================================================

    #[test]
    fn test_all_dialects_composite_key_consistency() {
        let composite_keys = &[
            "\"org_id\"".to_string(),
            "\"region\"".to_string(),
            "\"user_id\"".to_string(),
        ];

        for f in dialect_fixtures() {
            let token_val = f.dialect.binary_literal("aabbccdd");
            let upsert = upsert_if_newer_sql(
                f.dialect,
                f.fq_table,
                f.fq_tombstone,
                &[
                    "\"org_id\"".to_string(),
                    "\"region\"".to_string(),
                    "\"user_id\"".to_string(),
                    "\"name\"".to_string(),
                    "\"_skippr_order_token\"".to_string(),
                ],
                &[
                    "1".to_string(),
                    "'us-east'".to_string(),
                    "42".to_string(),
                    "'Alice'".to_string(),
                    token_val.clone(),
                ],
                composite_keys,
                "aabbccdd",
            );

            let delete = delete_if_newer_sql(
                f.dialect,
                f.fq_table,
                f.fq_tombstone,
                composite_keys,
                &["1".to_string(), "'us-east'".to_string(), "42".to_string()],
                &[
                    "BIGINT".to_string(),
                    "VARCHAR".to_string(),
                    "BIGINT".to_string(),
                ],
                "aabbccdd",
            );

            // All dialects must reference all 3 business key columns in ON/WHERE
            for key in composite_keys {
                let clean_key = key.trim_matches('"');
                assert!(
                    upsert.contains(clean_key),
                    "{:?} upsert missing business key {}",
                    f.dialect,
                    clean_key,
                );
                assert!(
                    delete.contains(clean_key),
                    "{:?} delete missing business key {}",
                    f.dialect,
                    clean_key,
                );
            }

            // Tombstone DDL composite key
            let ddl = ddl_create_tombstone_table(
                f.dialect,
                f.fq_tombstone,
                &[
                    ("org_id".to_string(), "BIGINT".to_string()),
                    ("region".to_string(), "VARCHAR".to_string()),
                    ("user_id".to_string(), "BIGINT".to_string()),
                ],
            );
            assert!(
                ddl.contains("\"org_id\"")
                    && ddl.contains("\"region\"")
                    && ddl.contains("\"user_id\""),
                "{:?} tombstone DDL missing composite key columns",
                f.dialect,
            );
        }
    }
}
