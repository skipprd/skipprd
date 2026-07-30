#![allow(dead_code)]

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
            .map(|(name, ty)| format!("\"{}\" {} NOT NULL", name.replace('"', "\"\""), ty))
            .collect();
        col_defs.push(format!(
            "\"_skippr_order_token\" {} NOT NULL",
            Self::ORDER_TOKEN_TYPE
        ));

        let pk_cols: Vec<String> = business_key_cols
            .iter()
            .map(|(name, _)| format!("\"{}\"", name.replace('"', "\"\"")))
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

/// Sink-neutral scalar values carried by a bounded CDC apply batch.
///
/// Values are kept out of backend SQL syntax so adapters can use native bulk
/// loading without interpolating one SQL statement per row.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum CdcApplyValue {
    Null,
    Boolean(bool),
    Signed(i64),
    Unsigned(u64),
    Float(String),
    Text(String),
    Binary(Vec<u8>),
    Date(String),
    Timestamp(String),
}

/// Target-column metadata required by a CDC reference adapter.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CdcApplyColumn {
    pub name: String,
    pub target_type: String,
}

/// Apply-level mutation after collapsing snapshot/insert/update to upsert.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CdcApplyMutation {
    Upsert,
    Delete,
}

/// Per-row metadata aligned with the row values in a `CdcApplyBatch`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CdcApplyRowMetadata {
    pub mutation: CdcApplyMutation,
    pub event_id: Vec<u8>,
    pub order_token: Vec<u8>,
    /// Stable input position used only to make equal-token replays
    /// deterministic while reducing a batch to one winner per business key.
    pub source_ordinal: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CdcApplyRow {
    pub metadata: CdcApplyRowMetadata,
    pub values: Vec<CdcApplyValue>,
}

/// A bounded, grouped CDC chunk ready for a sink reference adapter.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CdcApplyBatch {
    pub columns: Vec<CdcApplyColumn>,
    pub business_key_columns: Vec<String>,
    pub rows: Vec<CdcApplyRow>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CdcApplyBatchError {
    NoColumns,
    NoBusinessKeyColumns,
    NoRows,
    EmptyTargetType {
        column: String,
    },
    DuplicateColumn {
        column: String,
    },
    MissingBusinessKeyColumn {
        column: String,
    },
    RowWidth {
        row: usize,
        expected: usize,
        actual: usize,
    },
    EmptyOrderToken {
        row: usize,
    },
    DuplicateSourceOrdinal {
        ordinal: u64,
    },
    MissingRowMetadata {
        row: usize,
        available: usize,
    },
    WarehouseStageLimit {
        dialect: &'static str,
        resource: &'static str,
        actual: usize,
        limit: usize,
    },
}

impl std::fmt::Display for CdcApplyBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoColumns => write!(f, "CDC apply batch has no target columns"),
            Self::NoBusinessKeyColumns => {
                write!(f, "CDC apply batch has no business key columns")
            }
            Self::NoRows => write!(f, "CDC apply batch has no rows"),
            Self::EmptyTargetType { column } => {
                write!(f, "CDC target column {column:?} has no target type")
            }
            Self::DuplicateColumn { column } => {
                write!(f, "CDC target column {column:?} is duplicated")
            }
            Self::MissingBusinessKeyColumn { column } => {
                write!(f, "CDC business key column {column:?} is not in the batch")
            }
            Self::RowWidth {
                row,
                expected,
                actual,
            } => write!(f, "CDC row {row} has {actual} values; expected {expected}"),
            Self::EmptyOrderToken { row } => {
                write!(f, "CDC row {row} has an empty order token")
            }
            Self::DuplicateSourceOrdinal { ordinal } => {
                write!(f, "CDC source ordinal {ordinal} is duplicated")
            }
            Self::MissingRowMetadata { row, available } => write!(
                f,
                "CDC row metadata missing at index {row} (have {available})"
            ),
            Self::WarehouseStageLimit {
                dialect,
                resource,
                actual,
                limit,
            } => write!(
                f,
                "{dialect} CDC staging {resource} is {actual}; limit is {limit}"
            ),
        }
    }
}

impl std::error::Error for CdcApplyBatchError {}

impl CdcApplyBatch {
    pub fn validate(&self) -> Result<(), CdcApplyBatchError> {
        if self.columns.is_empty() {
            return Err(CdcApplyBatchError::NoColumns);
        }
        if self.business_key_columns.is_empty() {
            return Err(CdcApplyBatchError::NoBusinessKeyColumns);
        }

        let mut column_names = std::collections::HashSet::new();
        for column in &self.columns {
            if column.target_type.trim().is_empty() {
                return Err(CdcApplyBatchError::EmptyTargetType {
                    column: column.name.clone(),
                });
            }
            if !column_names.insert(column.name.as_str()) {
                return Err(CdcApplyBatchError::DuplicateColumn {
                    column: column.name.clone(),
                });
            }
        }
        for business_key in &self.business_key_columns {
            if !column_names.contains(business_key.as_str()) {
                return Err(CdcApplyBatchError::MissingBusinessKeyColumn {
                    column: business_key.clone(),
                });
            }
        }

        let mut ordinals = std::collections::HashSet::new();
        for (row_idx, row) in self.rows.iter().enumerate() {
            if row.values.len() != self.columns.len() {
                return Err(CdcApplyBatchError::RowWidth {
                    row: row_idx,
                    expected: self.columns.len(),
                    actual: row.values.len(),
                });
            }
            if row.metadata.order_token.is_empty() {
                return Err(CdcApplyBatchError::EmptyOrderToken { row: row_idx });
            }
            if !ordinals.insert(row.metadata.source_ordinal) {
                return Err(CdcApplyBatchError::DuplicateSourceOrdinal {
                    ordinal: row.metadata.source_ordinal,
                });
            }
        }

        Ok(())
    }
}

impl CdcApplyBatchError {
    pub fn is_warehouse_stage_limit(&self) -> bool {
        matches!(self, Self::WarehouseStageLimit { .. })
    }
}

/// Append an Arrow record batch and its aligned WAL metadata to the typed CDC
/// apply IR. Backends retain control of target names and target SQL types while
/// sharing one lossless scalar conversion and metadata-alignment check.
pub fn append_record_batch_to_cdc_apply(
    apply_batch: &mut CdcApplyBatch,
    record_batch: &datafusion::arrow::record_batch::RecordBatch,
    row_metadata: &[skippr_runtime_sdk::plugins::cdc::WalRowMeta],
    source_offset: usize,
) -> Result<(), CdcApplyBatchError> {
    use skippr_runtime_sdk::plugins::cdc::MutationKind;

    for row in 0..record_batch.num_rows() {
        let metadata_index = source_offset + row;
        let row_metadata =
            row_metadata
                .get(metadata_index)
                .ok_or(CdcApplyBatchError::MissingRowMetadata {
                    row: metadata_index,
                    available: row_metadata.len(),
                })?;
        let mutation = match row_metadata.mutation {
            MutationKind::Snapshot | MutationKind::Insert | MutationKind::Update => {
                CdcApplyMutation::Upsert
            }
            MutationKind::Delete => CdcApplyMutation::Delete,
        };
        let values = record_batch
            .columns()
            .iter()
            .map(|column| arrow_value_to_cdc_apply(column.as_ref(), row))
            .collect();
        apply_batch.rows.push(CdcApplyRow {
            metadata: CdcApplyRowMetadata {
                mutation,
                event_id: row_metadata.event_id.clone(),
                order_token: row_metadata.order_token.clone(),
                source_ordinal: metadata_index as u64,
            },
            values,
        });
    }

    Ok(())
}

fn arrow_value_to_cdc_apply(
    array: &dyn datafusion::arrow::array::Array,
    row: usize,
) -> CdcApplyValue {
    use datafusion::arrow::array::*;
    use datafusion::arrow::datatypes::DataType;

    if array.is_null(row) {
        return CdcApplyValue::Null;
    }

    match array.data_type() {
        DataType::Boolean => CdcApplyValue::Boolean(
            array
                .as_any()
                .downcast_ref::<BooleanArray>()
                .expect("boolean Arrow array")
                .value(row),
        ),
        DataType::Int8 => CdcApplyValue::Signed(
            array
                .as_any()
                .downcast_ref::<Int8Array>()
                .expect("int8 Arrow array")
                .value(row) as i64,
        ),
        DataType::Int16 => CdcApplyValue::Signed(
            array
                .as_any()
                .downcast_ref::<Int16Array>()
                .expect("int16 Arrow array")
                .value(row) as i64,
        ),
        DataType::Int32 => CdcApplyValue::Signed(
            array
                .as_any()
                .downcast_ref::<Int32Array>()
                .expect("int32 Arrow array")
                .value(row) as i64,
        ),
        DataType::Int64 => CdcApplyValue::Signed(
            array
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("int64 Arrow array")
                .value(row),
        ),
        DataType::UInt8 => CdcApplyValue::Unsigned(
            array
                .as_any()
                .downcast_ref::<UInt8Array>()
                .expect("uint8 Arrow array")
                .value(row) as u64,
        ),
        DataType::UInt16 => CdcApplyValue::Unsigned(
            array
                .as_any()
                .downcast_ref::<UInt16Array>()
                .expect("uint16 Arrow array")
                .value(row) as u64,
        ),
        DataType::UInt32 => CdcApplyValue::Unsigned(
            array
                .as_any()
                .downcast_ref::<UInt32Array>()
                .expect("uint32 Arrow array")
                .value(row) as u64,
        ),
        DataType::UInt64 => CdcApplyValue::Unsigned(
            array
                .as_any()
                .downcast_ref::<UInt64Array>()
                .expect("uint64 Arrow array")
                .value(row),
        ),
        DataType::Float16
        | DataType::Float32
        | DataType::Float64
        | DataType::Decimal128(_, _)
        | DataType::Decimal256(_, _) => CdcApplyValue::Float(
            datafusion::arrow::util::display::array_value_to_string(array, row).unwrap_or_default(),
        ),
        DataType::Utf8 => CdcApplyValue::Text(
            array
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("utf8 Arrow array")
                .value(row)
                .to_string(),
        ),
        DataType::LargeUtf8 => CdcApplyValue::Text(
            array
                .as_any()
                .downcast_ref::<LargeStringArray>()
                .expect("large utf8 Arrow array")
                .value(row)
                .to_string(),
        ),
        DataType::Binary => CdcApplyValue::Binary(
            array
                .as_any()
                .downcast_ref::<BinaryArray>()
                .expect("binary Arrow array")
                .value(row)
                .to_vec(),
        ),
        DataType::LargeBinary => CdcApplyValue::Binary(
            array
                .as_any()
                .downcast_ref::<LargeBinaryArray>()
                .expect("large binary Arrow array")
                .value(row)
                .to_vec(),
        ),
        DataType::Date32 | DataType::Date64 => CdcApplyValue::Date(
            datafusion::arrow::util::display::array_value_to_string(array, row).unwrap_or_default(),
        ),
        DataType::Timestamp(_, _) => CdcApplyValue::Timestamp(
            datafusion::arrow::util::display::array_value_to_string(array, row).unwrap_or_default(),
        ),
        _ => CdcApplyValue::Text(
            datafusion::arrow::util::display::array_value_to_string(array, row).unwrap_or_default(),
        ),
    }
}

/// SQL warehouse families whose exact-final-state adapters can atomically
/// update both the target and tombstone tables.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CdcWarehouseDialect {
    Snowflake,
    BigQuery,
    Redshift,
    Synapse,
    MotherDuck,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WarehouseCdcLimits {
    pub max_rows: usize,
    pub max_load_rows: usize,
    pub max_statement_bytes: usize,
    pub max_total_encoded_bytes: usize,
    pub max_statements: usize,
}

/// Set-based SQL for one bounded typed CDC batch. Setup statements create and
/// bulk-fill connection-local staging tables. Apply statements must execute in
/// one backend transaction. Cleanup is explicit on success; temporary-table
/// scope provides failure cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WarehouseBulkCdcSql {
    pub stage_table: String,
    pub winners_table: String,
    pub idempotency_key: String,
    pub setup_statements: Vec<String>,
    pub apply_statements: Vec<String>,
    pub cleanup_statements: Vec<String>,
}

impl WarehouseBulkCdcSql {
    /// Statements for APIs, such as Redshift BatchExecuteStatement, that wrap
    /// the supplied statement list in one transaction.
    pub fn atomic_statements(&self) -> Vec<String> {
        self.setup_statements
            .iter()
            .chain(self.apply_statements.iter())
            .chain(self.cleanup_statements.iter())
            .cloned()
            .collect()
    }

    /// One multi-statement request for connection-oriented warehouse APIs.
    pub fn transactional_script(&self, dialect: CdcWarehouseDialect) -> String {
        let setup = join_sql_statements(&self.setup_statements);
        let apply = join_sql_statements(&self.apply_statements);
        let cleanup = join_sql_statements(&self.cleanup_statements);

        match dialect {
            CdcWarehouseDialect::Snowflake => {
                format!("{setup}\nBEGIN TRANSACTION;\n{apply}\nCOMMIT;\n{cleanup}")
            }
            CdcWarehouseDialect::BigQuery => {
                format!("BEGIN TRANSACTION;\n{setup}\n{apply}\n{cleanup}\nCOMMIT TRANSACTION;")
            }
            CdcWarehouseDialect::Synapse => format!(
                "SET XACT_ABORT ON;\n\
                 BEGIN TRY\n\
                 {setup}\n\
                 BEGIN TRANSACTION;\n\
                 {apply}\n\
                 COMMIT TRANSACTION;\n\
                 {cleanup}\n\
                 END TRY\n\
                 BEGIN CATCH\n\
                 IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;\n\
                 {cleanup}\n\
                 THROW;\n\
                 END CATCH;"
            ),
            CdcWarehouseDialect::Redshift | CdcWarehouseDialect::MotherDuck => {
                format!("BEGIN TRANSACTION;\n{setup}\n{apply}\n{cleanup}\nCOMMIT;")
            }
        }
    }
}

/// Build explicitly bounded staging SQL and four set-based MERGEs:
/// target upserts, target deletes, tombstone advances, and stale-tombstone
/// cleanup. The winner relation collapses duplicate keys by descending order
/// token and ascending source ordinal, matching sequential equal-token
/// behavior deterministically.
pub fn warehouse_bulk_cdc_sql(
    dialect: CdcWarehouseDialect,
    fq_table: &str,
    fq_tombstone_table: &str,
    batch: &CdcApplyBatch,
) -> Result<WarehouseBulkCdcSql, CdcApplyBatchError> {
    use std::hash::{Hash, Hasher};

    batch.validate()?;
    if batch.rows.is_empty() {
        return Err(CdcApplyBatchError::NoRows);
    }
    let limits = dialect.staging_limits();
    dialect.validate_stage_row_count(batch.rows.len())?;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    dialect.hash(&mut hasher);
    fq_table.hash(&mut hasher);
    fq_tombstone_table.hash(&mut hasher);
    batch.hash(&mut hasher);
    let batch_hash = format!("{:016x}", hasher.finish());
    let suffix = &batch_hash[..12];
    let stage_name = dialect.temp_table_name(&format!("_skippr_cdc_stage_{suffix}"));
    let winners_name = dialect.temp_table_name(&format!("_skippr_cdc_winners_{suffix}"));

    let ordinal = dialect.quote_identifier("_skippr_cdc_ordinal");
    let mutation = dialect.quote_identifier("_skippr_cdc_mutation");
    let stage_token = dialect.quote_identifier("_skippr_cdc_order_token");
    let target_token = dialect.quote_identifier("_skippr_order_token");
    let rank = dialect.quote_identifier("_skippr_cdc_rank");
    let target_columns = batch
        .columns
        .iter()
        .map(|column| dialect.quote_identifier(&column.name))
        .collect::<Vec<_>>();
    let business_keys = batch
        .business_key_columns
        .iter()
        .map(|column| dialect.quote_identifier(column))
        .collect::<Vec<_>>();

    let stage_column_defs = [
        format!("{ordinal} {} NOT NULL", dialect.ordinal_type()),
        format!("{mutation} {} NOT NULL", dialect.mutation_type()),
        format!("{stage_token} {} NOT NULL", dialect.order_token_type()),
    ]
    .into_iter()
    .chain(
        batch
            .columns
            .iter()
            .zip(target_columns.iter())
            .map(|(column, quoted_name)| format!("{quoted_name} {}", column.target_type)),
    )
    .collect::<Vec<_>>()
    .join(", ");
    let create_stage = dialect.create_temp_table(&stage_name, &stage_column_defs);

    let load_columns = [ordinal.clone(), mutation.clone(), stage_token.clone()]
        .into_iter()
        .chain(target_columns.iter().cloned())
        .collect::<Vec<_>>()
        .join(", ");
    let load_prefix = format!("INSERT INTO {stage_name} ({load_columns}) VALUES\n");
    let mut load_rows = Vec::with_capacity(batch.rows.len());
    let mut encoded_row_bytes = 0usize;
    for row in &batch.rows {
        let mut values = vec![
            row.metadata.source_ordinal.to_string(),
            match row.metadata.mutation {
                CdcApplyMutation::Upsert => "0".to_string(),
                CdcApplyMutation::Delete => "1".to_string(),
            },
            dialect.binary_literal(&encode_hex_bytes(&row.metadata.order_token)),
        ];
        values.extend(
            row.values
                .iter()
                .zip(batch.columns.iter())
                .map(|(value, column)| dialect.value_literal(value, &column.target_type)),
        );
        let encoded_row = format!("({})", values.join(", "));
        let statement_bytes = encoded_row.len() + load_prefix.len() + 1;
        if statement_bytes > limits.max_statement_bytes {
            return Err(dialect.stage_limit_error(
                "single encoded row bytes",
                statement_bytes,
                limits.max_statement_bytes,
            ));
        }
        encoded_row_bytes = encoded_row_bytes
            .saturating_add(encoded_row.len() + usize::from(!load_rows.is_empty()) * 2);
        if encoded_row_bytes > limits.max_total_encoded_bytes {
            return Err(dialect.stage_limit_error(
                "encoded stage rows bytes",
                encoded_row_bytes,
                limits.max_total_encoded_bytes,
            ));
        }
        load_rows.push(encoded_row);
    }
    let mut load_stage = Vec::new();
    let mut chunk = Vec::new();
    let mut chunk_bytes = load_prefix.len() + 1;
    for row in load_rows {
        let separator_bytes = usize::from(!chunk.is_empty()) * 2;
        let row_bytes = row.len() + separator_bytes;
        if !chunk.is_empty()
            && (chunk.len() >= limits.max_load_rows
                || chunk_bytes + row_bytes > limits.max_statement_bytes)
        {
            load_stage.push(format!("{load_prefix}{};", chunk.join(",\n")));
            chunk.clear();
            chunk_bytes = load_prefix.len() + 1;
        }
        chunk_bytes += row.len() + usize::from(!chunk.is_empty()) * 2;
        chunk.push(row);
    }
    if !chunk.is_empty() {
        load_stage.push(format!("{load_prefix}{};", chunk.join(",\n")));
    }

    let winner_projection = target_columns
        .iter()
        .map(|column| format!("ranked.{column}"))
        .chain([
            format!("ranked.{ordinal}"),
            format!("ranked.{mutation}"),
            format!("ranked.{stage_token}"),
        ])
        .collect::<Vec<_>>()
        .join(", ");
    let partition_by = business_keys
        .iter()
        .map(|column| format!("stage.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    let winner_select = format!(
        "SELECT {winner_projection}\n\
         FROM (\n\
           SELECT stage.*,\n\
             ROW_NUMBER() OVER (\n\
               PARTITION BY {partition_by}\n\
               ORDER BY stage.{stage_token} DESC, stage.{ordinal} ASC\n\
             ) AS {rank}\n\
           FROM {stage_name} AS stage\n\
         ) AS ranked\n\
         WHERE ranked.{rank} = 1"
    );
    let create_winners = dialect.create_temp_table_as(&winners_name, &winner_select);

    let target_to_source = key_match_aliases("target", "source", &business_keys);
    let tombstone_to_winner = key_match_aliases("tombstone", "winner", &business_keys);
    let target_update_assignments = target_columns
        .iter()
        .filter(|column| !business_keys.contains(column))
        .map(|column| format!("{column} = source.{column}"))
        .chain(std::iter::once(format!(
            "{target_token} = source.{stage_token}"
        )))
        .collect::<Vec<_>>()
        .join(", ");
    let target_insert_columns = target_columns
        .iter()
        .cloned()
        .chain(std::iter::once(target_token.clone()))
        .collect::<Vec<_>>()
        .join(", ");
    let target_insert_values = target_columns
        .iter()
        .map(|column| format!("source.{column}"))
        .chain(std::iter::once(format!("source.{stage_token}")))
        .collect::<Vec<_>>()
        .join(", ");

    let upsert_source = format!(
        "SELECT winner.*\n\
         FROM {winners_name} AS winner\n\
         WHERE winner.{mutation} = 0\n\
         AND NOT EXISTS (\n\
           SELECT 1 FROM {fq_tombstone_table} AS tombstone\n\
           WHERE {tombstone_to_winner}\n\
           AND tombstone.{target_token} >= winner.{stage_token}\n\
         )"
    );
    let apply_upserts = format!(
        "MERGE INTO {fq_table} AS target\n\
         USING ({upsert_source}) AS source\n\
         ON {target_to_source}\n\
         WHEN MATCHED AND (\n\
           target.{target_token} IS NULL\n\
           OR target.{target_token} < source.{stage_token}\n\
         ) THEN UPDATE SET {target_update_assignments}\n\
         WHEN NOT MATCHED THEN INSERT ({target_insert_columns})\n\
         VALUES ({target_insert_values});"
    );

    let delete_source = format!("SELECT * FROM {winners_name} WHERE {mutation} = 1");
    let apply_deletes = format!(
        "MERGE INTO {fq_table} AS target\n\
         USING ({delete_source}) AS source\n\
         ON {target_to_source}\n\
         WHEN MATCHED AND (\n\
           target.{target_token} IS NULL\n\
           OR target.{target_token} < source.{stage_token}\n\
         ) THEN DELETE;"
    );

    let tombstone_to_source = key_match_aliases("tombstone", "source", &business_keys);
    let tombstone_columns = business_keys
        .iter()
        .cloned()
        .chain(std::iter::once(target_token.clone()))
        .collect::<Vec<_>>()
        .join(", ");
    let tombstone_values = business_keys
        .iter()
        .map(|column| format!("source.{column}"))
        .chain(std::iter::once(format!("source.{stage_token}")))
        .collect::<Vec<_>>()
        .join(", ");
    let apply_tombstones = format!(
        "MERGE INTO {fq_tombstone_table} AS tombstone\n\
         USING ({delete_source}) AS source\n\
         ON {tombstone_to_source}\n\
         WHEN MATCHED AND tombstone.{target_token} < source.{stage_token}\n\
           THEN UPDATE SET {target_token} = source.{stage_token}\n\
         WHEN NOT MATCHED THEN INSERT ({tombstone_columns})\n\
           VALUES ({tombstone_values});"
    );

    let upsert_winners = format!("SELECT * FROM {winners_name} WHERE {mutation} = 0");
    let clear_stale_tombstones = format!(
        "MERGE INTO {fq_tombstone_table} AS tombstone\n\
         USING ({upsert_winners}) AS source\n\
         ON {tombstone_to_source}\n\
         WHEN MATCHED AND tombstone.{target_token} < source.{stage_token}\n\
           THEN DELETE;"
    );

    let apply_statements = if dialect == CdcWarehouseDialect::Redshift {
        let winner_to_target = key_match_aliases("target", "winner", &business_keys);
        let redshift_update_assignments = target_columns
            .iter()
            .filter(|column| !business_keys.contains(column))
            .map(|column| format!("{column} = winner.{column}"))
            .chain(std::iter::once(format!(
                "{target_token} = winner.{stage_token}"
            )))
            .collect::<Vec<_>>()
            .join(", ");
        let update_upserts = format!(
            "UPDATE {fq_table} AS target\n\
             SET {redshift_update_assignments}\n\
             FROM {winners_name} AS winner\n\
             WHERE winner.{mutation} = 0\n\
             AND {winner_to_target}\n\
             AND (\n\
               target.{target_token} IS NULL\n\
               OR target.{target_token} < winner.{stage_token}\n\
             )\n\
             AND NOT EXISTS (\n\
               SELECT 1 FROM {fq_tombstone_table} AS tombstone\n\
               WHERE {tombstone_to_winner}\n\
               AND tombstone.{target_token} >= winner.{stage_token}\n\
             );"
        );
        let insert_upserts = format!(
            "INSERT INTO {fq_table} ({target_insert_columns})\n\
             SELECT {target_insert_values}\n\
             FROM {winners_name} AS source\n\
             WHERE source.{mutation} = 0\n\
             AND NOT EXISTS (\n\
               SELECT 1 FROM {fq_table} AS target\n\
               WHERE {target_to_source}\n\
             )\n\
             AND NOT EXISTS (\n\
               SELECT 1 FROM {fq_tombstone_table} AS tombstone\n\
               WHERE {tombstone_to_source}\n\
               AND tombstone.{target_token} >= source.{stage_token}\n\
             );"
        );
        let delete_targets = format!(
            "DELETE FROM {fq_table} AS target\n\
             USING {winners_name} AS winner\n\
             WHERE winner.{mutation} = 1\n\
             AND {winner_to_target}\n\
             AND (\n\
               target.{target_token} IS NULL\n\
               OR target.{target_token} < winner.{stage_token}\n\
             );"
        );
        let update_tombstones = format!(
            "UPDATE {fq_tombstone_table} AS tombstone\n\
             SET {target_token} = winner.{stage_token}\n\
             FROM {winners_name} AS winner\n\
             WHERE winner.{mutation} = 1\n\
             AND {tombstone_to_winner}\n\
             AND tombstone.{target_token} < winner.{stage_token};"
        );
        let insert_tombstones = format!(
            "INSERT INTO {fq_tombstone_table} ({tombstone_columns})\n\
             SELECT {tombstone_values}\n\
             FROM {winners_name} AS source\n\
             WHERE source.{mutation} = 1\n\
             AND NOT EXISTS (\n\
               SELECT 1 FROM {fq_tombstone_table} AS tombstone\n\
               WHERE {tombstone_to_source}\n\
             );"
        );
        let clear_tombstones = format!(
            "DELETE FROM {fq_tombstone_table} AS tombstone\n\
             USING {winners_name} AS winner\n\
             WHERE winner.{mutation} = 0\n\
             AND {tombstone_to_winner}\n\
             AND tombstone.{target_token} < winner.{stage_token};"
        );
        vec![
            update_upserts,
            insert_upserts,
            delete_targets,
            update_tombstones,
            insert_tombstones,
            clear_tombstones,
        ]
    } else {
        vec![
            apply_upserts,
            apply_deletes,
            apply_tombstones,
            clear_stale_tombstones,
        ]
    };

    let sql = WarehouseBulkCdcSql {
        stage_table: stage_name.clone(),
        winners_table: winners_name.clone(),
        idempotency_key: format!("skippr-cdc-{batch_hash}"),
        setup_statements: std::iter::once(create_stage)
            .chain(load_stage)
            .chain(std::iter::once(create_winners))
            .collect(),
        apply_statements,
        cleanup_statements: vec![
            format!("DROP TABLE IF EXISTS {winners_name};"),
            format!("DROP TABLE IF EXISTS {stage_name};"),
        ],
    };
    if let Some(oversized) = sql
        .atomic_statements()
        .into_iter()
        .map(|statement| statement.len())
        .find(|bytes| *bytes > limits.max_statement_bytes)
    {
        return Err(dialect.stage_limit_error(
            "statement bytes",
            oversized,
            limits.max_statement_bytes,
        ));
    }
    let statement_count = sql.atomic_statements().len();
    if statement_count > limits.max_statements {
        return Err(dialect.stage_limit_error(
            "statement count",
            statement_count,
            limits.max_statements,
        ));
    }
    let encoded_bytes = match dialect {
        CdcWarehouseDialect::Redshift => sql
            .atomic_statements()
            .iter()
            .map(|statement| statement.len())
            .sum(),
        _ => sql.transactional_script(dialect).len(),
    };
    if encoded_bytes > limits.max_total_encoded_bytes {
        return Err(dialect.stage_limit_error(
            "total encoded bytes",
            encoded_bytes,
            limits.max_total_encoded_bytes,
        ));
    }

    Ok(sql)
}

/// Rebuild the pre-bulk guarded path from the typed IR. Each row remains an
/// independent replay-safe transaction, so adapters can fall back when a
/// bounded chunk exceeds a backend request or statement envelope.
pub fn guarded_warehouse_cdc_sql<B: CdcApplyBackend>(
    dialect: CdcWarehouseDialect,
    fq_table: &str,
    fq_tombstone_table: &str,
    batch: &CdcApplyBatch,
) -> Result<Vec<String>, CdcApplyBatchError> {
    batch.validate()?;

    let target_columns = batch
        .columns
        .iter()
        .map(|column| dialect.quote_identifier(&column.name))
        .collect::<Vec<_>>();
    let business_key_columns = batch
        .business_key_columns
        .iter()
        .map(|column| dialect.quote_identifier(column))
        .collect::<Vec<_>>();
    let business_key_indexes = batch
        .business_key_columns
        .iter()
        .map(|business_key| {
            batch
                .columns
                .iter()
                .position(|column| column.name == *business_key)
                .expect("validated business key column")
        })
        .collect::<Vec<_>>();
    let business_key_types = business_key_indexes
        .iter()
        .map(|index| batch.columns[*index].target_type.clone())
        .collect::<Vec<_>>();
    let target_order_token = dialect.quote_identifier("_skippr_order_token");

    Ok(batch
        .rows
        .iter()
        .map(|row| {
            let order_token_hex = encode_hex_bytes(&row.metadata.order_token);
            match row.metadata.mutation {
                CdcApplyMutation::Upsert => {
                    let mut names = target_columns.clone();
                    names.push(target_order_token.clone());
                    let mut values = row
                        .values
                        .iter()
                        .zip(batch.columns.iter())
                        .map(|(value, column)| dialect.value_literal(value, &column.target_type))
                        .collect::<Vec<_>>();
                    values.push(B::binary_literal(&order_token_hex));
                    B::upsert_if_newer_sql(
                        fq_table,
                        fq_tombstone_table,
                        &names,
                        &values,
                        &business_key_columns,
                        &order_token_hex,
                    )
                }
                CdcApplyMutation::Delete => {
                    let values = business_key_indexes
                        .iter()
                        .map(|index| {
                            dialect.value_literal(
                                &row.values[*index],
                                &batch.columns[*index].target_type,
                            )
                        })
                        .collect::<Vec<_>>();
                    B::delete_if_newer_sql(
                        fq_table,
                        fq_tombstone_table,
                        &business_key_columns,
                        &values,
                        &business_key_types,
                        &order_token_hex,
                    )
                }
            }
        })
        .collect())
}

pub fn guarded_warehouse_cdc_row_sql<B: CdcApplyBackend>(
    dialect: CdcWarehouseDialect,
    fq_table: &str,
    fq_tombstone_table: &str,
    batch: &CdcApplyBatch,
    row: &CdcApplyRow,
) -> Result<String, CdcApplyBatchError> {
    let single_row_batch = CdcApplyBatch {
        columns: batch.columns.clone(),
        business_key_columns: batch.business_key_columns.clone(),
        rows: vec![row.clone()],
    };
    Ok(
        guarded_warehouse_cdc_sql::<B>(dialect, fq_table, fq_tombstone_table, &single_row_batch)?
            .into_iter()
            .next()
            .expect("single-row guarded batch produces one statement"),
    )
}

impl std::hash::Hash for CdcWarehouseDialect {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&(*self as u8), state);
    }
}

impl CdcWarehouseDialect {
    pub fn name(self) -> &'static str {
        match self {
            Self::Snowflake => "Snowflake",
            Self::BigQuery => "BigQuery",
            Self::Redshift => "Redshift",
            Self::Synapse => "Synapse",
            Self::MotherDuck => "MotherDuck",
        }
    }

    pub fn staging_limits(self) -> WarehouseCdcLimits {
        match self {
            // Snowflake recommends keeping the complete multi-statement SQL
            // API request below 1 MiB so query text remains retryable.
            Self::Snowflake => WarehouseCdcLimits {
                max_rows: 10_000,
                max_load_rows: 500,
                max_statement_bytes: 128 * 1024,
                max_total_encoded_bytes: 900 * 1024,
                max_statements: 64,
            },
            // BigQuery caps the unresolved text of the complete script at
            // 1 MiB, including comments and whitespace.
            Self::BigQuery => WarehouseCdcLimits {
                max_rows: 10_000,
                max_load_rows: 500,
                max_statement_bytes: 128 * 1024,
                max_total_encoded_bytes: 900 * 1024,
                max_statements: 64,
            },
            // Redshift Data API caps each SQL statement at 100 KiB and
            // BatchExecuteStatement at 40 statements.
            Self::Redshift => WarehouseCdcLimits {
                max_rows: 15_000,
                max_load_rows: 500,
                max_statement_bytes: 90 * 1024,
                max_total_encoded_bytes: 30 * 90 * 1024,
                max_statements: 40,
            },
            // Dedicated Synapse accepts at most 1,000 VALUES rows per INSERT
            // and a 256 MiB TDS batch at the default packet size. Stay well
            // below the byte ceiling while still admitting narrow 100k chunks.
            Self::Synapse => WarehouseCdcLimits {
                max_rows: 100_000,
                max_load_rows: 1_000,
                max_statement_bytes: 1024 * 1024,
                max_total_encoded_bytes: 64 * 1024 * 1024,
                max_statements: 128,
            },
            // The current MotherDuck HTTP connector exposes no appender or
            // documented request-size contract, so use a deliberately small
            // defensive envelope and fall back outside it.
            Self::MotherDuck => WarehouseCdcLimits {
                max_rows: 1_000,
                max_load_rows: 250,
                max_statement_bytes: 128 * 1024,
                max_total_encoded_bytes: 512 * 1024,
                max_statements: 16,
            },
        }
    }

    pub fn validate_stage_row_count(self, row_count: usize) -> Result<(), CdcApplyBatchError> {
        let limit = self.staging_limits().max_rows;
        if row_count > limit {
            return Err(self.stage_limit_error("row count", row_count, limit));
        }
        Ok(())
    }

    fn stage_limit_error(
        self,
        resource: &'static str,
        actual: usize,
        limit: usize,
    ) -> CdcApplyBatchError {
        CdcApplyBatchError::WarehouseStageLimit {
            dialect: self.name(),
            resource,
            actual,
            limit,
        }
    }

    fn quote_identifier(self, identifier: &str) -> String {
        match self {
            Self::BigQuery => format!("`{}`", identifier.replace('`', "\\`")),
            Self::Synapse => format!("[{}]", identifier.replace(']', "]]")),
            _ => format!("\"{}\"", identifier.replace('"', "\"\"")),
        }
    }

    fn temp_table_name(self, identifier: &str) -> String {
        match self {
            Self::Synapse => self.quote_identifier(&format!("#{identifier}")),
            _ => self.quote_identifier(identifier),
        }
    }

    fn create_temp_table(self, table: &str, column_defs: &str) -> String {
        match self {
            Self::Snowflake => {
                format!("CREATE TEMPORARY TABLE {table} ({column_defs});")
            }
            Self::BigQuery | Self::Redshift | Self::MotherDuck => {
                format!("CREATE TEMP TABLE {table} ({column_defs});")
            }
            Self::Synapse => format!(
                "CREATE TABLE {table} ({column_defs}) \
                 WITH (DISTRIBUTION = ROUND_ROBIN, HEAP);"
            ),
        }
    }

    fn create_temp_table_as(self, table: &str, select: &str) -> String {
        match self {
            Self::Snowflake => format!("CREATE TEMPORARY TABLE {table} AS\n{select};"),
            Self::BigQuery | Self::Redshift | Self::MotherDuck => {
                format!("CREATE TEMP TABLE {table} AS\n{select};")
            }
            Self::Synapse => format!(
                "CREATE TABLE {table}\n\
                 WITH (DISTRIBUTION = ROUND_ROBIN, HEAP)\n\
                 AS {select};"
            ),
        }
    }

    fn ordinal_type(self) -> &'static str {
        match self {
            Self::BigQuery => "INT64",
            _ => "BIGINT",
        }
    }

    fn mutation_type(self) -> &'static str {
        match self {
            Self::BigQuery => "INT64",
            _ => "SMALLINT",
        }
    }

    fn order_token_type(self) -> &'static str {
        match self {
            Self::Snowflake => "BINARY",
            Self::BigQuery => "BYTES",
            Self::Redshift => "VARBYTE",
            Self::Synapse => "VARBINARY(MAX)",
            Self::MotherDuck => "BLOB",
        }
    }

    fn binary_literal(self, hex: &str) -> String {
        match self {
            Self::Snowflake => format!("HEX_DECODE_BINARY('{hex}')"),
            Self::BigQuery | Self::Redshift => format!("FROM_HEX('{hex}')"),
            Self::Synapse => format!("CONVERT(VARBINARY(MAX), 0x{hex})"),
            Self::MotherDuck => format!("'\\x{hex}'::BLOB"),
        }
    }

    fn value_literal(self, value: &CdcApplyValue, target_type: &str) -> String {
        match value {
            CdcApplyValue::Null => "NULL".to_string(),
            CdcApplyValue::Boolean(value) if self == Self::Synapse => {
                if *value { "1" } else { "0" }.to_string()
            }
            CdcApplyValue::Boolean(value) => if *value { "TRUE" } else { "FALSE" }.to_string(),
            CdcApplyValue::Signed(value) => value.to_string(),
            CdcApplyValue::Unsigned(value) => value.to_string(),
            CdcApplyValue::Float(value) => value.clone(),
            CdcApplyValue::Text(value) => sql_string_literal(value),
            CdcApplyValue::Binary(value) => self.binary_literal(&encode_hex_bytes(value)),
            CdcApplyValue::Date(value) | CdcApplyValue::Timestamp(value) => {
                format!("CAST({} AS {target_type})", sql_string_literal(value))
            }
        }
    }
}

fn join_sql_statements(statements: &[String]) -> String {
    statements.join("\n")
}

fn key_match_aliases(left: &str, right: &str, business_keys: &[String]) -> String {
    business_keys
        .iter()
        .map(|column| format!("{left}.{column} = {right}.{column}"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn encode_hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
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
pub fn warehouse_sql_test_batch() -> CdcApplyBatch {
    CdcApplyBatch {
        columns: vec![
            CdcApplyColumn {
                name: "tenant_id".to_string(),
                target_type: "BIGINT".to_string(),
            },
            CdcApplyColumn {
                name: "id".to_string(),
                target_type: "BIGINT".to_string(),
            },
            CdcApplyColumn {
                name: "value".to_string(),
                target_type: "VARCHAR".to_string(),
            },
        ],
        business_key_columns: vec!["tenant_id".to_string(), "id".to_string()],
        rows: vec![
            CdcApplyRow {
                metadata: CdcApplyRowMetadata {
                    mutation: CdcApplyMutation::Upsert,
                    event_id: b"upsert".to_vec(),
                    order_token: vec![1],
                    source_ordinal: 0,
                },
                values: vec![
                    CdcApplyValue::Signed(7),
                    CdcApplyValue::Signed(42),
                    CdcApplyValue::Text("O'Brien".to_string()),
                ],
            },
            CdcApplyRow {
                metadata: CdcApplyRowMetadata {
                    mutation: CdcApplyMutation::Delete,
                    event_id: b"delete".to_vec(),
                    order_token: vec![2],
                    source_ordinal: 1,
                },
                values: vec![
                    CdcApplyValue::Signed(7),
                    CdcApplyValue::Signed(42),
                    CdcApplyValue::Null,
                ],
            },
        ],
    }
}

#[cfg(test)]
pub fn warehouse_sql_test_batch_with_rows(row_count: usize, value_bytes: usize) -> CdcApplyBatch {
    let mut batch = warehouse_sql_test_batch();
    let value = "x".repeat(value_bytes);
    batch.rows = (0..row_count)
        .map(|ordinal| CdcApplyRow {
            metadata: CdcApplyRowMetadata {
                mutation: if ordinal % 7 == 0 {
                    CdcApplyMutation::Delete
                } else {
                    CdcApplyMutation::Upsert
                },
                event_id: format!("event-{ordinal}").into_bytes(),
                order_token: (ordinal as u64).to_be_bytes().to_vec(),
                source_ordinal: ordinal as u64,
            },
            values: vec![
                CdcApplyValue::Signed(7),
                CdcApplyValue::Signed(ordinal as i64),
                CdcApplyValue::Text(value.clone()),
            ],
        })
        .collect();
    batch
}

#[cfg(test)]
mod tests {
    use super::{
        guarded_warehouse_cdc_sql, tombstone_table_name, warehouse_bulk_cdc_sql,
        warehouse_sql_test_batch_with_rows, CdcApplyBackend, CdcApplyBatch, CdcApplyBatchError,
        CdcApplyColumn, CdcApplyMutation, CdcApplyRow, CdcApplyRowMetadata, CdcApplyValue,
        CdcWarehouseDialect,
    };

    struct TestBackend;

    impl CdcApplyBackend for TestBackend {
        const ORDER_TOKEN_TYPE: &'static str = "BINARY";

        fn binary_literal(hex: &str) -> String {
            format!("X'{hex}'")
        }
    }

    fn contract_row(
        mutation: CdcApplyMutation,
        ordinal: u64,
        token: u8,
        tenant_id: i64,
        id: i64,
        value: &str,
    ) -> CdcApplyRow {
        CdcApplyRow {
            metadata: CdcApplyRowMetadata {
                mutation,
                event_id: format!("event-{ordinal}").into_bytes(),
                order_token: vec![token],
                source_ordinal: ordinal,
            },
            values: vec![
                CdcApplyValue::Signed(tenant_id),
                CdcApplyValue::Signed(id),
                CdcApplyValue::Text(value.to_string()),
            ],
        }
    }

    fn contract_batch(rows: Vec<CdcApplyRow>) -> CdcApplyBatch {
        CdcApplyBatch {
            columns: vec![
                CdcApplyColumn {
                    name: "tenant_id".to_string(),
                    target_type: "BIGINT".to_string(),
                },
                CdcApplyColumn {
                    name: "id".to_string(),
                    target_type: "BIGINT".to_string(),
                },
                CdcApplyColumn {
                    name: "value".to_string(),
                    target_type: "VARCHAR".to_string(),
                },
            ],
            business_key_columns: vec!["tenant_id".to_string(), "id".to_string()],
            rows,
        }
    }

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

    #[test]
    fn typed_batch_validates_row_alignment_and_business_keys() {
        let batch = CdcApplyBatch {
            columns: vec![
                CdcApplyColumn {
                    name: "tenant_id".to_string(),
                    target_type: "BIGINT".to_string(),
                },
                CdcApplyColumn {
                    name: "id".to_string(),
                    target_type: "BIGINT".to_string(),
                },
            ],
            business_key_columns: vec!["tenant_id".to_string(), "id".to_string()],
            rows: vec![CdcApplyRow {
                metadata: CdcApplyRowMetadata {
                    mutation: CdcApplyMutation::Upsert,
                    event_id: b"event-1".to_vec(),
                    order_token: vec![1],
                    source_ordinal: 0,
                },
                values: vec![CdcApplyValue::Signed(7), CdcApplyValue::Signed(42)],
            }],
        };

        assert_eq!(batch.validate(), Ok(()));
    }

    #[test]
    fn typed_batch_rejects_bad_row_width() {
        let batch = CdcApplyBatch {
            columns: vec![CdcApplyColumn {
                name: "id".to_string(),
                target_type: "BIGINT".to_string(),
            }],
            business_key_columns: vec!["id".to_string()],
            rows: vec![CdcApplyRow {
                metadata: CdcApplyRowMetadata {
                    mutation: CdcApplyMutation::Delete,
                    event_id: b"event-1".to_vec(),
                    order_token: vec![1],
                    source_ordinal: 0,
                },
                values: vec![],
            }],
        };

        assert_eq!(
            batch.validate(),
            Err(CdcApplyBatchError::RowWidth {
                row: 0,
                expected: 1,
                actual: 0,
            })
        );
    }

    #[test]
    fn golden_contract_covers_mixed_replay_stale_composite_and_equal_tokens() {
        use std::collections::BTreeMap;

        let batch = contract_batch(vec![
            contract_row(CdcApplyMutation::Upsert, 0, 1, 10, 1, "first"),
            contract_row(CdcApplyMutation::Upsert, 1, 1, 10, 1, "equal replay"),
            contract_row(CdcApplyMutation::Upsert, 2, 0, 10, 1, "stale"),
            contract_row(CdcApplyMutation::Delete, 3, 3, 10, 1, ""),
            contract_row(CdcApplyMutation::Upsert, 4, 2, 10, 1, "late zombie"),
            contract_row(CdcApplyMutation::Upsert, 5, 4, 10, 1, "resurrected"),
            contract_row(CdcApplyMutation::Upsert, 6, 2, 20, 1, "other tenant"),
        ]);

        let mut winners: BTreeMap<(i64, i64), &CdcApplyRow> = BTreeMap::new();
        for row in &batch.rows {
            let key = match (&row.values[0], &row.values[1]) {
                (CdcApplyValue::Signed(tenant), CdcApplyValue::Signed(id)) => (*tenant, *id),
                _ => unreachable!(),
            };
            let replace = winners.get(&key).map_or(true, |winner| {
                row.metadata.order_token > winner.metadata.order_token
                    || (row.metadata.order_token == winner.metadata.order_token
                        && row.metadata.source_ordinal < winner.metadata.source_ordinal)
            });
            if replace {
                winners.insert(key, row);
            }
        }

        let primary = winners.get(&(10, 1)).unwrap();
        assert_eq!(primary.metadata.order_token, vec![4]);
        assert_eq!(primary.metadata.mutation, CdcApplyMutation::Upsert);
        assert_eq!(
            primary.values[2],
            CdcApplyValue::Text("resurrected".to_string())
        );
        assert_eq!(winners.get(&(20, 1)).unwrap().metadata.order_token, vec![2]);

        let equal_only = contract_batch(vec![
            contract_row(CdcApplyMutation::Delete, 8, 7, 30, 1, ""),
            contract_row(CdcApplyMutation::Upsert, 9, 7, 30, 1, "later ordinal"),
        ]);
        let first = equal_only
            .rows
            .iter()
            .min_by(|left, right| {
                right
                    .metadata
                    .order_token
                    .cmp(&left.metadata.order_token)
                    .then_with(|| {
                        left.metadata
                            .source_ordinal
                            .cmp(&right.metadata.source_ordinal)
                    })
            })
            .unwrap();
        assert_eq!(first.metadata.mutation, CdcApplyMutation::Delete);
    }

    #[test]
    fn warehouse_sql_contract_has_one_winner_relation_and_atomic_cleanup() {
        let batch = contract_batch(vec![
            contract_row(CdcApplyMutation::Upsert, 0, 1, 10, 1, "first"),
            contract_row(CdcApplyMutation::Delete, 1, 2, 10, 1, ""),
        ]);

        for dialect in [
            CdcWarehouseDialect::Snowflake,
            CdcWarehouseDialect::BigQuery,
            CdcWarehouseDialect::Redshift,
            CdcWarehouseDialect::Synapse,
            CdcWarehouseDialect::MotherDuck,
        ] {
            let sql = warehouse_bulk_cdc_sql(dialect, "target", "tombstones", &batch).unwrap();
            let setup = sql.setup_statements.join("\n");
            let apply = sql.apply_statements.join("\n");
            let cleanup = sql.cleanup_statements.join("\n");
            assert!(setup.contains("ROW_NUMBER() OVER"));
            assert!(setup.contains("_skippr_cdc_order_token"));
            assert!(setup.contains("_skippr_cdc_ordinal"));
            assert!(setup.contains("tenant_id"));
            assert!(setup.contains("id"));
            if dialect == CdcWarehouseDialect::Redshift {
                assert_eq!(apply.matches("MERGE INTO").count(), 0);
                assert!(apply.contains("UPDATE target AS target"));
                assert!(apply.contains("DELETE FROM tombstones AS tombstone"));
            } else {
                assert_eq!(apply.matches("MERGE INTO").count(), 4);
            }
            assert!(apply.contains("tombstone"));
            assert!(apply.contains(">="));
            assert_eq!(cleanup.matches("DROP TABLE IF EXISTS").count(), 2);
        }

        let synapse = warehouse_bulk_cdc_sql(
            CdcWarehouseDialect::Synapse,
            "[dbo].[target]",
            "[dbo].[tombstones]",
            &batch,
        )
        .unwrap()
        .transactional_script(CdcWarehouseDialect::Synapse);
        assert!(synapse.contains("ROLLBACK TRANSACTION"));
        assert!(synapse.contains("BEGIN CATCH"));
    }

    #[test]
    fn warehouse_limits_are_explicit_per_dialect() {
        let snowflake = CdcWarehouseDialect::Snowflake.staging_limits();
        assert_eq!(snowflake.max_total_encoded_bytes, 900 * 1024);
        assert_eq!(snowflake.max_load_rows, 500);

        let bigquery = CdcWarehouseDialect::BigQuery.staging_limits();
        assert_eq!(bigquery.max_total_encoded_bytes, 900 * 1024);
        assert_eq!(bigquery.max_load_rows, 500);

        let redshift = CdcWarehouseDialect::Redshift.staging_limits();
        assert_eq!(redshift.max_statement_bytes, 90 * 1024);
        assert_eq!(redshift.max_statements, 40);

        let synapse = CdcWarehouseDialect::Synapse.staging_limits();
        assert_eq!(synapse.max_rows, 100_000);
        assert_eq!(synapse.max_load_rows, 1_000);

        let motherduck = CdcWarehouseDialect::MotherDuck.staging_limits();
        assert_eq!(motherduck.max_rows, 1_000);
        assert_eq!(motherduck.max_total_encoded_bytes, 512 * 1024);
    }

    #[test]
    fn load_rows_split_at_dialect_statement_boundary() {
        let batch = warehouse_sql_test_batch_with_rows(501, 8);
        let sql = warehouse_bulk_cdc_sql(
            CdcWarehouseDialect::BigQuery,
            "`p.d.t`",
            "`p.d.tombstones`",
            &batch,
        )
        .unwrap();
        assert_eq!(
            sql.setup_statements
                .iter()
                .filter(|statement| statement.starts_with("INSERT INTO"))
                .count(),
            2
        );
        assert!(sql.setup_statements.iter().all(|statement| statement.len()
            <= CdcWarehouseDialect::BigQuery
                .staging_limits()
                .max_statement_bytes));
    }

    #[test]
    fn oversized_stage_fails_closed_to_guarded_sql() {
        let batch = warehouse_sql_test_batch_with_rows(2, 200 * 1024);
        let error = warehouse_bulk_cdc_sql(
            CdcWarehouseDialect::MotherDuck,
            "\"target\"",
            "\"tombstones\"",
            &batch,
        )
        .unwrap_err();
        assert!(error.is_warehouse_stage_limit());

        let guarded = guarded_warehouse_cdc_sql::<TestBackend>(
            CdcWarehouseDialect::MotherDuck,
            "\"target\"",
            "\"tombstones\"",
            &batch,
        )
        .unwrap();
        assert_eq!(guarded.len(), batch.rows.len());
        assert!(guarded.iter().all(|statement| statement.contains("BEGIN;")));
    }
}
