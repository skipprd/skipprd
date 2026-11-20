use crate::converters::skippr_arrow::EMPTY_STRUCT_MARKER_FIELD;
use crate::discover::schema_alter::{CanonicalAlter, CommittedAlter, OutputName};
use crate::discover::{OutputMetadata, SkipprDataType};
use crate::runtime_plugins::protocol::{RuntimeSchemaState, SchemaDelta};
use async_trait::async_trait;
use datafusion::arrow::datatypes::Schema as ArrowSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WarehouseDialect {
    Postgres,
    Snowflake,
    BigQuery,
    Redshift,
    MotherDuck,
}

/// Warehouse I/O behind the shared ALTER. Implemented by each warehouse sink.
#[async_trait]
pub trait WarehouseExec: Send + Sync {
    /// Column names of the namespace's table as the warehouse reports them.
    async fn columns(&self, namespace: &str) -> io::Result<Vec<String>>;
    async fn execute(&self, sql: &str) -> io::Result<()>;
    /// First cell of the first row, as text. Used to read `skippr_alter_seq`.
    async fn query_first(&self, sql: &str) -> io::Result<Option<String>>;
}

/// Dest high-water. Missing/blank is unset (seq 0 applies). Garbage fails closed.
pub fn parse_dest_high_water(raw: Option<&str>) -> io::Result<Option<u64>> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    raw.parse().map(Some).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("dest alter seq {raw:?} is not u64: {err}"),
        )
    })
}

/// The only warehouse ALTER path. `CommittedAlter.seq` is the destination
/// high-water (`skippr_alter_seq`). Statements already reflected in the
/// table's columns are also skipped, so a crash between DDL and the stamp
/// still resumes.
pub async fn apply_warehouse_alter(
    exec: &dyn WarehouseExec,
    dialect: WarehouseDialect,
    fq_table: &str,
    namespace: &str,
    alter: &CommittedAlter,
) -> io::Result<()> {
    let rel = seq_relation(dialect, fq_table);
    exec.execute(&create_alter_seq_sql(dialect, &rel)).await?;
    let current = parse_dest_high_water(
        exec.query_first(&read_alter_seq_sql(&rel, namespace))
            .await?
            .as_deref(),
    )?;
    if current.is_some_and(|seq| seq >= alter.seq) {
        return Ok(());
    }
    let existing = exec.columns(namespace).await?;
    for sql in warehouse_alter_sql(dialect, fq_table, &alter.alter, &existing)? {
        exec.execute(&sql).await?;
    }
    for sql in stamp_alter_seq_sql(dialect, &rel, namespace, alter.seq) {
        exec.execute(&sql).await?;
    }
    Ok(())
}

fn seq_relation(dialect: WarehouseDialect, fq_table: &str) -> String {
    let name = quote(dialect, "skippr_alter_seq");
    match fq_table.rsplit_once('.') {
        Some((prefix, _)) => format!("{prefix}.{name}"),
        None => name,
    }
}

fn create_alter_seq_sql(dialect: WarehouseDialect, rel: &str) -> String {
    match dialect {
        WarehouseDialect::BigQuery => {
            format!("CREATE TABLE IF NOT EXISTS {rel} (namespace STRING, seq INT64)")
        }
        WarehouseDialect::Snowflake => {
            format!("CREATE TABLE IF NOT EXISTS {rel} (namespace VARCHAR PRIMARY KEY, seq NUMBER)")
        }
        _ => format!(
            "CREATE TABLE IF NOT EXISTS {rel} (namespace VARCHAR(256) PRIMARY KEY, seq BIGINT NOT NULL)"
        ),
    }
}

fn read_alter_seq_sql(rel: &str, namespace: &str) -> String {
    let ns = namespace.replace('\'', "''");
    format!("SELECT CAST(seq AS VARCHAR) FROM {rel} WHERE namespace = '{ns}'")
}

fn stamp_alter_seq_sql(
    dialect: WarehouseDialect,
    rel: &str,
    namespace: &str,
    seq: u64,
) -> Vec<String> {
    let ns = namespace.replace('\'', "''");
    match dialect {
        WarehouseDialect::Postgres | WarehouseDialect::MotherDuck => vec![format!(
            "INSERT INTO {rel} (namespace, seq) VALUES ('{ns}', {seq}) ON CONFLICT (namespace) DO UPDATE SET seq = EXCLUDED.seq"
        )],
        WarehouseDialect::Redshift => vec![
            format!("DELETE FROM {rel} WHERE namespace = '{ns}'"),
            format!("INSERT INTO {rel} (namespace, seq) VALUES ('{ns}', {seq})"),
        ],
        WarehouseDialect::Snowflake | WarehouseDialect::BigQuery => vec![format!(
            "MERGE INTO {rel} t USING (SELECT '{ns}' AS namespace, {seq} AS seq) s ON t.namespace = s.namespace WHEN MATCHED THEN UPDATE SET seq = s.seq WHEN NOT MATCHED THEN INSERT (namespace, seq) VALUES (s.namespace, s.seq)"
        )],
    }
}

pub fn warehouse_alter_sql(
    dialect: WarehouseDialect,
    fq_table: &str,
    alter: &CanonicalAlter,
    existing: &[String],
) -> io::Result<Vec<String>> {
    let has = |name: &OutputName| {
        let want = fold(dialect, name.column());
        existing.iter().any(|column| fold(dialect, column) == want)
    };
    let col = |name: &OutputName| quote(dialect, name.column());
    let drop = |name: &OutputName| format!("ALTER TABLE {fq_table} DROP COLUMN {}", col(name));
    Ok(match alter {
        CanonicalAlter::Rename { column, .. } => {
            let to = alter
                .rename_target()
                .map_err(invalid)?
                .unwrap_or_else(|| column.name.clone());
            if !has(&column.name) && has(&to) {
                Vec::new()
            } else {
                vec![format!(
                    "ALTER TABLE {fq_table} RENAME COLUMN {} TO {}",
                    col(&column.name),
                    col(&to)
                )]
            }
        }
        CanonicalAlter::Drop { column } if has(&column.name) => vec![drop(&column.name)],
        CanonicalAlter::Drop { .. } => Vec::new(),
        CanonicalAlter::Merge { src, dst } if has(&src.name) => vec![
            format!(
                "UPDATE {fq_table} SET {dst} = COALESCE({dst}, CAST({src} AS {ty})) WHERE {src} IS NOT NULL",
                dst = col(&dst.name),
                src = col(&src.name),
                ty = sql_type(dialect, &dst.ty)?,
            ),
            drop(&src.name),
        ],
        CanonicalAlter::Merge { .. } => Vec::new(),
        CanonicalAlter::Promote { column, to } => {
            let ty = sql_type(dialect, to)?;
            let column = col(&column.name);
            vec![match dialect {
                WarehouseDialect::Snowflake | WarehouseDialect::BigQuery => {
                    format!("ALTER TABLE {fq_table} ALTER COLUMN {column} SET DATA TYPE {ty}")
                }
                _ => format!("ALTER TABLE {fq_table} ALTER COLUMN {column} TYPE {ty}"),
            }]
        }
    })
}

fn invalid(err: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, err)
}

/// Scalar dest column from installed `OutputMetadata`. Nested fields fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedColumn {
    pub name: String,
    pub ty: SkipprDataType,
    pub field_id: i32,
}

/// Installed dest schema. Publish reads this; it does not read Arrow batches.
#[derive(Clone, Debug, Default)]
pub struct InstalledDestSchema {
    installed: bool,
    version: u64,
    namespaces: BTreeMap<String, OutputMetadata>,
    versions: BTreeMap<String, u64>,
}

impl InstalledDestSchema {
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn namespace_versions(&self) -> &BTreeMap<String, u64> {
        &self.versions
    }

    pub fn install_state(
        &mut self,
        schema_version: u64,
        namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> bool {
        if self.installed && schema_version <= self.version {
            return false;
        }
        self.installed = true;
        self.version = schema_version;
        self.versions = namespaces
            .keys()
            .map(|namespace| (namespace.clone(), schema_version))
            .collect();
        self.namespaces = namespaces.clone();
        true
    }

    pub fn install_snapshot(&mut self, state: &RuntimeSchemaState) -> bool {
        if self.installed && state.version <= self.version {
            return false;
        }
        self.installed = true;
        self.version = state.version;
        self.namespaces = state.namespaces.clone();
        self.versions = state.namespace_versions.clone();
        true
    }

    pub fn install_delta(&mut self, delta: &SchemaDelta) -> Vec<String> {
        let mut changed = Vec::new();
        for (namespace, entry) in &delta.namespaces {
            if self
                .versions
                .get(namespace)
                .is_some_and(|installed| *installed >= entry.version)
            {
                continue;
            }
            self.namespaces
                .insert(namespace.clone(), entry.metadata.clone());
            self.versions.insert(namespace.clone(), entry.version);
            changed.push(namespace.clone());
        }
        self.installed = true;
        self.version = self.version.max(delta.version);
        changed
    }

    pub fn get(&self, namespace: &str) -> io::Result<(u64, OutputMetadata)> {
        let metadata = self.metadata(namespace)?.clone();
        let version = self.versions.get(namespace).copied().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("published dest schema is missing a version for namespace '{namespace}'"),
            )
        })?;
        Ok((version, metadata))
    }

    pub fn metadata(&self, namespace: &str) -> io::Result<&OutputMetadata> {
        self.namespaces.get(namespace).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("published dest schema is missing namespace '{namespace}'"),
            )
        })
    }

    pub fn batch_columns(
        &self,
        namespace: &str,
        batch: &ArrowSchema,
        case: IdentCase,
        sql_type: impl Fn(&SkipprDataType) -> io::Result<&'static str>,
    ) -> io::Result<PublishedBatchColumns> {
        PublishedBatchColumns::new(self.metadata(namespace)?, batch, case, sql_type)
    }

    pub fn warehouse_batch_columns(
        &self,
        dialect: WarehouseDialect,
        namespace: &str,
        batch: &ArrowSchema,
    ) -> io::Result<PublishedBatchColumns> {
        self.batch_columns(namespace, batch, dialect.ident_case(), |ty| {
            sql_type(dialect, ty)
        })
    }
}

/// `(namespace → schema_version)` already published. A hit runs no DDL.
#[derive(Debug, Default)]
pub struct DestPublishCache {
    versions: std::sync::Mutex<BTreeMap<String, u64>>,
    in_flight: std::sync::Mutex<BTreeMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

impl DestPublishCache {
    /// Runs `publish` unless `(namespace, schema_version)` is already published.
    /// One publish per namespace runs at a time; the version is recorded only
    /// after `publish` succeeds.
    pub async fn publish<Fut>(
        &self,
        namespace: &str,
        schema_version: u64,
        publish: impl FnOnce() -> Fut,
    ) -> io::Result<()>
    where
        Fut: std::future::Future<Output = io::Result<()>>,
    {
        if self.hit(namespace, schema_version) {
            return Ok(());
        }
        let lock = std::sync::Arc::clone(
            self.in_flight
                .lock()
                .expect("dest publish cache poisoned")
                .entry(namespace.to_string())
                .or_default(),
        );
        let _guard = lock.lock().await;
        if self.hit(namespace, schema_version) {
            return Ok(());
        }
        publish().await?;
        self.versions
            .lock()
            .expect("dest publish cache poisoned")
            .entry(namespace.to_string())
            .and_modify(|published| *published = (*published).max(schema_version))
            .or_insert(schema_version);
        Ok(())
    }

    fn hit(&self, namespace: &str, schema_version: u64) -> bool {
        self.versions
            .lock()
            .expect("dest publish cache poisoned")
            .get(namespace)
            .is_some_and(|published| *published >= schema_version)
    }
}

/// Columns of one namespace. Child fields are the table. A nested child fails closed.
pub fn published_columns(metadata: &OutputMetadata) -> io::Result<Vec<PublishedColumn>> {
    let mut columns = Vec::new();
    for (key, child) in metadata.child_fields() {
        if child.child_fields().next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("published column '{key}' is nested; warehouse publish does not flatten"),
            ));
        }
        let name = published_name(key, child);
        if name.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("published column '{key}' has an empty name"),
            ));
        }
        columns.push(PublishedColumn {
            name,
            ty: child.determined_type().clone(),
            field_id: child.field_id(),
        });
    }
    columns.sort_by(|left, right| {
        left.field_id
            .cmp(&right.field_id)
            .then_with(|| left.name.cmp(&right.name))
    });
    if columns.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "published dest schema has no columns",
        ));
    }
    Ok(columns)
}

/// CREATE when the table is missing. ADD only for names the table does not have.
/// An existing column is left as-is: type changes and renames are `apply_alter`.
pub fn warehouse_publish_sql(
    dialect: WarehouseDialect,
    fq_table: &str,
    metadata: &OutputMetadata,
    existing: &[String],
) -> io::Result<Vec<String>> {
    let columns = published_columns_folded(metadata, dialect.ident_case())?;
    if existing.is_empty() {
        let defs = columns
            .iter()
            .map(|column| {
                Ok(format!(
                    "{} {}",
                    quote(dialect, &column.name),
                    sql_type(dialect, &column.ty)?
                ))
            })
            .collect::<io::Result<Vec<_>>>()?;
        return Ok(vec![format!(
            "CREATE TABLE IF NOT EXISTS {fq_table} ({})",
            defs.join(", ")
        )]);
    }
    missing_published_columns(&columns, existing, dialect.ident_case())
        .into_iter()
        .map(|column| add_column_sql(dialect, fq_table, &column.name, &column.ty))
        .collect()
}

/// `published_columns` for a destination that compares names under `case`.
/// Two published names that fold to one destination name fail closed.
pub fn published_columns_folded(
    metadata: &OutputMetadata,
    case: IdentCase,
) -> io::Result<Vec<PublishedColumn>> {
    let columns = published_columns(metadata)?;
    let mut seen = BTreeMap::<String, &str>::new();
    for column in &columns {
        if let Some(previous) = seen.insert(case.fold(&column.name), &column.name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "published columns '{}' and '{}' fold to the same destination name",
                    previous, column.name
                ),
            ));
        }
    }
    Ok(columns)
}

/// Publish post-condition: the live table carries every published column.
pub fn require_published_columns_live(
    columns: &[PublishedColumn],
    live: &[String],
    case: IdentCase,
) -> io::Result<()> {
    match missing_published_columns(columns, live, case).first() {
        Some(column) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "dest table is missing published column '{}' after publish",
                column.name
            ),
        )),
        None => Ok(()),
    }
}

/// Published columns the live table does not have, compared under `case`.
pub fn missing_published_columns<'a>(
    columns: &'a [PublishedColumn],
    existing: &[String],
    case: IdentCase,
) -> Vec<&'a PublishedColumn> {
    let have: Vec<String> = existing.iter().map(|name| case.fold(name)).collect();
    columns
        .iter()
        .filter(|column| !have.contains(&case.fold(&column.name)))
        .collect()
}

fn add_column_sql(
    dialect: WarehouseDialect,
    fq_table: &str,
    name: &str,
    ty: &SkipprDataType,
) -> io::Result<String> {
    let column = quote(dialect, name);
    let sql_ty = sql_type(dialect, ty)?;
    Ok(match dialect {
        WarehouseDialect::Redshift => {
            format!("ALTER TABLE {fq_table} ADD COLUMN {column} {sql_ty}")
        }
        _ => format!("ALTER TABLE {fq_table} ADD COLUMN IF NOT EXISTS {column} {sql_ty}"),
    })
}

/// One publish per `(namespace, schema_version)`. A cache hit executes no SQL.
pub async fn publish_warehouse_table(
    cache: &DestPublishCache,
    exec: &dyn WarehouseExec,
    dialect: WarehouseDialect,
    fq_table: &str,
    namespace: &str,
    schema_version: u64,
    metadata: &OutputMetadata,
) -> io::Result<()> {
    cache
        .publish(namespace, schema_version, || async {
            let existing = exec.columns(namespace).await?;
            for sql in warehouse_publish_sql(dialect, fq_table, metadata, &existing)? {
                exec.execute(&sql).await?;
            }
            let columns = published_columns(metadata)?;
            require_published_columns_live(
                &columns,
                &exec.columns(namespace).await?,
                dialect.ident_case(),
            )
        })
        .await
}

/// Batch columns in batch order, folded and typed from published metadata.
/// Carries the fold so business keys resolve with the same identifier case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedBatchColumns {
    case: IdentCase,
    columns: Vec<(String, &'static str)>,
}

impl PublishedBatchColumns {
    /// A batch column that metadata does not contain fails closed. Metadata
    /// columns absent from the batch are omitted.
    pub fn new(
        metadata: &OutputMetadata,
        batch: &ArrowSchema,
        case: IdentCase,
        sql_type: impl Fn(&SkipprDataType) -> io::Result<&'static str>,
    ) -> io::Result<Self> {
        let published = published_columns(metadata)?;
        let mut columns = Vec::with_capacity(batch.fields().len());
        for field in batch.fields() {
            let want = case.fold(field.name());
            let Some(found) = published
                .iter()
                .find(|column| case.fold(&column.name) == want)
            else {
                return Err(unpublished_batch_column(field.name()));
            };
            columns.push((want, sql_type(&found.ty)?));
        }
        Ok(Self { case, columns })
    }

    pub fn columns(&self) -> &[(String, &'static str)] {
        &self.columns
    }

    /// Business keys as published `(name, sql_type)`. A missing key fails closed.
    pub fn key_types(&self, keys: &[String]) -> io::Result<Vec<(String, &'static str)>> {
        keys.iter()
            .map(|key| {
                let want = self.case.fold(key);
                self.columns
                    .iter()
                    .find(|(name, _)| *name == want)
                    .cloned()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("business key '{key}' is not in the published schema"),
                        )
                    })
            })
            .collect()
    }
}

/// Batch names only, for sinks that keep nested published columns (Hive).
/// The empty-struct placeholder column is not a published name.
pub fn reject_unpublished_batch_columns(
    metadata: &OutputMetadata,
    batch: &ArrowSchema,
) -> io::Result<()> {
    let have: Vec<String> = metadata
        .child_fields()
        .map(|(key, child)| published_name(key, child))
        .collect();
    reject_batch_columns_missing_from(batch, |name| have.iter().any(|have| have == name))
}

/// Every batch column except the empty-struct placeholder must satisfy `has`.
pub fn reject_batch_columns_missing_from(
    batch: &ArrowSchema,
    has: impl Fn(&str) -> bool,
) -> io::Result<()> {
    match batch
        .fields()
        .iter()
        .map(|field| field.name())
        .find(|name| name.as_str() != EMPTY_STRUCT_MARKER_FIELD && !has(name))
    {
        Some(name) => Err(unpublished_batch_column(name)),
        None => Ok(()),
    }
}

fn unpublished_batch_column(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("batch column '{name}' is not in the published schema"),
    )
}

fn published_name(key: &str, child: &OutputMetadata) -> String {
    if child.out_field_name().is_empty() {
        key.to_string()
    } else {
        child.out_field_name().to_string()
    }
}

/// Table name for a namespace: dots become underscores, lowercase.
pub fn dest_table_name(namespace: &str) -> String {
    namespace.replace('.', "_").to_lowercase()
}

/// First column of a warehouse SQL JSON body (Snowflake `data`, BigQuery/MotherDuck `rows`).
pub fn json_result_names(body: &serde_json::Value) -> io::Result<Vec<String>> {
    json_rows(body)?
        .iter()
        .map(|row| {
            first_cell(row)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "warehouse column row has no cell",
                )
            })
        })
        .collect()
}

/// First cell of the first row as text. Numbers stringify. Missing `data`/`rows`
/// array fails closed. A present empty array is unset. Unreadable cells fail closed.
pub fn json_result_first(body: &serde_json::Value) -> io::Result<Option<String>> {
    match json_rows(body)?.first() {
        None => Ok(None),
        Some(row) => first_cell(row),
    }
}

fn json_rows(body: &serde_json::Value) -> io::Result<&[serde_json::Value]> {
    match body.get("data").or_else(|| body.get("rows")) {
        Some(serde_json::Value::Array(rows)) => Ok(rows.as_slice()),
        Some(other) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("warehouse result data/rows is not an array: {other}"),
        )),
        // BigQuery omits `rows` for an empty result and reports `totalRows`.
        None if body.get("totalRows").and_then(serde_json::Value::as_str) == Some("0") => Ok(&[]),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("warehouse result has no data/rows array: {body}"),
        )),
    }
}

fn first_cell(row: &serde_json::Value) -> io::Result<Option<String>> {
    if let Some(name) = row.as_str() {
        return Ok(Some(name.to_string()));
    }
    if let Some(cells) = row.as_array() {
        return match cells.first() {
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "warehouse result row is an empty array",
            )),
            Some(cell) => json_cell_text(cell).map(Some),
        };
    }
    if let Some(cell) = row.pointer("/f/0/v") {
        return json_cell_text(cell).map(Some);
    }
    match row.as_object().map(|record| record.values().collect::<Vec<_>>()) {
        Some(values) if values.len() == 1 => json_cell_text(values[0]).map(Some),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "warehouse result row is not a scalar, array, single-column record, or BigQuery cell: {row}"
            ),
        )),
    }
}

fn json_cell_text(value: &serde_json::Value) -> io::Result<String> {
    match value {
        serde_json::Value::String(text) => Ok(text.clone()),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        serde_json::Value::Bool(flag) => Ok(flag.to_string()),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("warehouse cell is not text or number: {other}"),
        )),
    }
}

/// Identifier case a destination stores for unquoted-equivalent names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentCase {
    Exact,
    Lower,
    Upper,
}

impl IdentCase {
    pub fn fold(self, ident: &str) -> String {
        match self {
            Self::Exact => ident.to_string(),
            Self::Lower => ident.to_lowercase(),
            Self::Upper => ident.to_uppercase(),
        }
    }
}

impl WarehouseDialect {
    /// Identifier case as each warehouse sink creates its columns.
    pub fn ident_case(self) -> IdentCase {
        match self {
            Self::Snowflake => IdentCase::Upper,
            Self::Postgres | Self::Redshift | Self::BigQuery | Self::MotherDuck => IdentCase::Lower,
        }
    }

    /// Folded, quoted identifier as this warehouse's sink writes it.
    pub fn quote_ident(self, ident: &str) -> String {
        quote(self, ident)
    }
}

fn fold(dialect: WarehouseDialect, ident: &str) -> String {
    dialect.ident_case().fold(ident)
}

fn quote(dialect: WarehouseDialect, ident: &str) -> String {
    let ident = fold(dialect, ident);
    match dialect {
        WarehouseDialect::BigQuery => format!("`{}`", ident.replace('`', "\\`")),
        _ => format!("\"{}\"", ident.replace('"', "\"\"")),
    }
}

fn sql_type(dialect: WarehouseDialect, ty: &SkipprDataType) -> io::Result<&'static str> {
    use WarehouseDialect::*;
    Ok(match ty {
        SkipprDataType::Long => match dialect {
            BigQuery => "INT64",
            Snowflake => "NUMBER",
            _ => "BIGINT",
        },
        SkipprDataType::Byte | SkipprDataType::Short | SkipprDataType::Integer => match dialect {
            BigQuery => "INT64",
            Snowflake => "NUMBER",
            _ => "INTEGER",
        },
        SkipprDataType::Double | SkipprDataType::Float => match dialect {
            BigQuery => "FLOAT64",
            Snowflake => "FLOAT",
            _ => "DOUBLE PRECISION",
        },
        SkipprDataType::Timestamp | SkipprDataType::TimestampMilli => match dialect {
            Snowflake => "TIMESTAMP_NTZ",
            _ => "TIMESTAMP",
        },
        SkipprDataType::Date => "DATE",
        SkipprDataType::Boolean => match dialect {
            BigQuery => "BOOL",
            _ => "BOOLEAN",
        },
        SkipprDataType::String => match dialect {
            BigQuery => "STRING",
            Snowflake => "VARCHAR",
            Redshift => "VARCHAR(65535)",
            Postgres | MotherDuck => "TEXT",
        },
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("warehouse ALTER has no SQL type for {ty}"),
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discover::schema_alter::{AlterColumn, FlattenMode};

    const ALL: [WarehouseDialect; 5] = [
        WarehouseDialect::Postgres,
        WarehouseDialect::Snowflake,
        WarehouseDialect::BigQuery,
        WarehouseDialect::Redshift,
        WarehouseDialect::MotherDuck,
    ];

    fn column(name: &str, id: i32, ty: SkipprDataType) -> AlterColumn {
        AlterColumn {
            name: OutputName::parse_sql(FlattenMode::Flat, name).unwrap(),
            id,
            ty,
        }
    }

    fn merge() -> CanonicalAlter {
        CanonicalAlter::Merge {
            src: column("price_string", 9, SkipprDataType::String),
            dst: column("price", 7, SkipprDataType::String),
        }
    }

    fn rename() -> CanonicalAlter {
        CanonicalAlter::Rename {
            column: column("region", 21, SkipprDataType::String),
            to_leaf: "region_code".into(),
        }
    }

    fn cols(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn postgres_rename_quotes_identifiers() {
        let sql = warehouse_alter_sql(
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            &rename(),
            &cols(&["region"]),
        )
        .unwrap();
        assert_eq!(
            sql,
            vec!["ALTER TABLE \"public\".\"trips\" RENAME COLUMN \"region\" TO \"region_code\""]
        );
    }

    #[test]
    fn postgres_merge_is_update_then_drop() {
        let sql = warehouse_alter_sql(
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            &merge(),
            &cols(&["price", "price_string"]),
        )
        .unwrap();
        assert_eq!(
            sql,
            vec![
                "UPDATE \"public\".\"trips\" SET \"price\" = COALESCE(\"price\", CAST(\"price_string\" AS TEXT)) WHERE \"price_string\" IS NOT NULL",
                "ALTER TABLE \"public\".\"trips\" DROP COLUMN \"price_string\"",
            ]
        );
    }

    #[test]
    fn merge_casts_to_dst_type_for_every_dialect() {
        let alter = CanonicalAlter::Merge {
            src: column("n", 3, SkipprDataType::Integer),
            dst: column("n_string", 4, SkipprDataType::String),
        };
        for dialect in ALL {
            let sql = warehouse_alter_sql(dialect, "t", &alter, &cols(&["n", "n_string"])).unwrap();
            assert_eq!(sql.len(), 2, "{dialect:?}");
            assert!(sql[0].contains("COALESCE"), "{dialect:?} {sql:?}");
            assert!(sql[0].contains(sql_type(dialect, &SkipprDataType::String).unwrap()));
            assert!(sql[1].contains("DROP COLUMN"), "{dialect:?} {sql:?}");
        }
    }

    #[test]
    fn resume_skips_statements_already_applied() {
        for dialect in ALL {
            assert!(
                warehouse_alter_sql(dialect, "t", &rename(), &cols(&["region_code"]))
                    .unwrap()
                    .is_empty(),
                "{dialect:?}"
            );
            assert!(
                warehouse_alter_sql(dialect, "t", &merge(), &cols(&["price"]))
                    .unwrap()
                    .is_empty()
            );
            let drop = CanonicalAlter::Drop {
                column: column("note", 8, SkipprDataType::String),
            };
            assert!(warehouse_alter_sql(dialect, "t", &drop, &cols(&["price"]))
                .unwrap()
                .is_empty());
        }
    }

    #[test]
    fn identifiers_fold_like_the_sink_creates_them() {
        let snowflake = warehouse_alter_sql(
            WarehouseDialect::Snowflake,
            "t",
            &merge(),
            &cols(&["PRICE_STRING"]),
        )
        .unwrap();
        assert!(snowflake[0].contains("\"PRICE\""), "{snowflake:?}");
        assert!(snowflake[0].contains("VARCHAR"), "{snowflake:?}");
        let bigquery = warehouse_alter_sql(
            WarehouseDialect::BigQuery,
            "t",
            &merge(),
            &cols(&["price_string"]),
        )
        .unwrap();
        assert!(bigquery[0].contains("`price`"), "{bigquery:?}");
    }

    /// Applies its own publish DDL to `cols` unless `frozen`.
    struct FakeExec {
        cols: std::sync::Mutex<Vec<String>>,
        frozen: bool,
        seq: std::sync::Mutex<Option<String>>,
        ddl: std::sync::Mutex<Vec<String>>,
    }

    impl FakeExec {
        fn new(cols: Vec<String>, seq: Option<String>) -> Self {
            Self {
                cols: std::sync::Mutex::new(cols),
                frozen: false,
                seq: std::sync::Mutex::new(seq),
                ddl: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn apply_publish_ddl(&self, sql: &str) {
            let quoted = |text: &str| -> Vec<String> {
                text.split('"')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_string)
                    .collect()
            };
            let mut cols = self.cols.lock().unwrap();
            if let Some((_, defs)) = sql
                .strip_prefix("CREATE TABLE IF NOT EXISTS")
                .and_then(|rest| rest.split_once('('))
            {
                cols.extend(quoted(defs));
            } else if let Some((_, column)) = sql.split_once("ADD COLUMN") {
                cols.extend(quoted(column).into_iter().take(1));
            }
        }
    }

    #[async_trait]
    impl WarehouseExec for FakeExec {
        async fn columns(&self, _namespace: &str) -> io::Result<Vec<String>> {
            Ok(self.cols.lock().unwrap().clone())
        }
        async fn execute(&self, sql: &str) -> io::Result<()> {
            self.ddl.lock().unwrap().push(sql.to_string());
            if !self.frozen && !sql.contains("skippr_alter_seq") {
                self.apply_publish_ddl(sql);
            }
            if sql.contains("skippr_alter_seq") && !sql.contains("CREATE TABLE") {
                if let Some(seq) = sql
                    .split(|ch: char| !ch.is_ascii_digit())
                    .filter_map(|part| part.parse::<u64>().ok())
                    .last()
                {
                    *self.seq.lock().unwrap() = Some(seq.to_string());
                }
            }
            Ok(())
        }
        async fn query_first(&self, _sql: &str) -> io::Result<Option<String>> {
            Ok(self.seq.lock().unwrap().clone())
        }
    }

    fn committed(alter: CanonicalAlter, seq: u64) -> CommittedAlter {
        use crate::discover::schema_alter::{AlterOrigin, MigrationDirection, MigrationId};
        CommittedAlter {
            origin: AlterOrigin {
                migration: MigrationId::new("t"),
                direction: MigrationDirection::Up,
            },
            seq,
            alter,
        }
    }

    #[tokio::test]
    async fn missing_seq_applies_then_high_water_skips() {
        let exec = FakeExec::new(cols(&["region"]), None);
        apply_warehouse_alter(
            &exec,
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            "trips",
            &committed(rename(), 0),
        )
        .await
        .unwrap();
        let first = exec.ddl.lock().unwrap().clone();
        assert!(
            first.iter().any(|sql| sql.contains("RENAME COLUMN")),
            "{first:?}"
        );
        exec.ddl.lock().unwrap().clear();
        apply_warehouse_alter(
            &exec,
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            "trips",
            &committed(rename(), 0),
        )
        .await
        .unwrap();
        let retry = exec.ddl.lock().unwrap().clone();
        assert!(
            retry.iter().all(|sql| !sql.contains("RENAME COLUMN")),
            "{retry:?}"
        );
    }

    #[tokio::test]
    async fn garbage_seq_fails_closed_before_ddl() {
        let exec = FakeExec::new(cols(&["region"]), Some("nope".into()));
        let err = apply_warehouse_alter(
            &exec,
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            "trips",
            &committed(rename(), 0),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not u64"), "{err}");
        let ddl = exec.ddl.lock().unwrap().clone();
        assert!(
            ddl.iter().all(|sql| !sql.contains("RENAME COLUMN")),
            "{ddl:?}"
        );
    }

    #[test]
    fn parse_dest_high_water_missing_is_unset_garbage_fails() {
        assert_eq!(parse_dest_high_water(None).unwrap(), None);
        assert_eq!(parse_dest_high_water(Some("")).unwrap(), None);
        assert_eq!(parse_dest_high_water(Some("  ")).unwrap(), None);
        assert_eq!(parse_dest_high_water(Some("0")).unwrap(), Some(0));
        assert!(parse_dest_high_water(Some("nope")).is_err());
    }

    #[test]
    fn json_result_first_stringifies_numbers() {
        let snowflake = serde_json::json!({"data": [[0]]});
        assert_eq!(json_result_first(&snowflake).unwrap().as_deref(), Some("0"));
        let named = serde_json::json!({"data": ["region"]});
        assert_eq!(
            json_result_first(&named).unwrap().as_deref(),
            Some("region")
        );
        let empty = serde_json::json!({"data": []});
        assert_eq!(json_result_first(&empty).unwrap(), None);
        let object = serde_json::json!({"data": [[{"seq": 1}]]});
        assert!(json_result_first(&object).is_err());
        assert!(json_result_first(&serde_json::json!({"message": "denied"})).is_err());
        assert!(json_result_first(&serde_json::json!({"data": {"seq": 1}})).is_err());
        assert_eq!(
            json_result_first(&serde_json::json!({"data": [{"seq": 1}]}))
                .unwrap()
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            json_result_names(&serde_json::json!({"data": [{"column_name": "region"}]})).unwrap(),
            vec!["region"]
        );
        assert!(json_result_first(&serde_json::json!({"data": [{"a": 1, "b": 2}]})).is_err());
        assert!(json_result_names(&serde_json::json!({"message": "denied"})).is_err());
        assert!(json_result_names(&serde_json::json!({"totalRows": "0"}))
            .unwrap()
            .is_empty());
        assert!(json_result_names(&serde_json::json!({"totalRows": "2"})).is_err());
        assert_eq!(
            json_result_names(&serde_json::json!({"data": ["region"]})).unwrap(),
            vec!["region"]
        );
    }

    fn output_with(columns: &[(&str, i32, SkipprDataType)]) -> OutputMetadata {
        let mut root = OutputMetadata::new();
        let mut fields = std::collections::HashMap::new();
        for (name, id, ty) in columns {
            let mut column = OutputMetadata::new();
            column.out_field_name = (*name).to_string();
            column.determined_type = ty.clone();
            column.field_id = *id;
            fields.insert((*name).to_string(), column);
        }
        root.fields = Box::new(fields);
        root
    }

    #[test]
    fn publish_sql_uses_metadata_types_and_does_not_retype() {
        let metadata = output_with(&[
            ("id", 1, SkipprDataType::Long),
            ("region", 2, SkipprDataType::String),
        ]);
        let created = warehouse_publish_sql(
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            &metadata,
            &[],
        )
        .unwrap();
        assert_eq!(
            created,
            vec![
                "CREATE TABLE IF NOT EXISTS \"public\".\"trips\" (\"id\" BIGINT, \"region\" TEXT)"
            ]
        );
        let added = warehouse_publish_sql(
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            &metadata,
            &cols(&["id"]),
        )
        .unwrap();
        assert_eq!(
            added,
            vec!["ALTER TABLE \"public\".\"trips\" ADD COLUMN IF NOT EXISTS \"region\" TEXT"]
        );
        assert!(
            added.iter().all(|sql| !sql.contains("CREATE TABLE")),
            "{added:?}"
        );
        assert!(
            added
                .iter()
                .all(|sql| !sql.contains("TYPE") && !sql.contains("SET DATA TYPE")),
            "{added:?}"
        );
        let snowflake = warehouse_publish_sql(
            WarehouseDialect::Snowflake,
            "\"DB\".\"PUBLIC\".\"TRIPS\"",
            &metadata,
            &cols(&["ID"]),
        )
        .unwrap();
        assert_eq!(
            snowflake,
            vec!["ALTER TABLE \"DB\".\"PUBLIC\".\"TRIPS\" ADD COLUMN IF NOT EXISTS \"REGION\" VARCHAR"]
        );
    }

    #[test]
    fn redshift_strings_are_full_width_varchar() {
        assert_eq!(
            sql_type(WarehouseDialect::Redshift, &SkipprDataType::String).unwrap(),
            "VARCHAR(65535)"
        );
        assert_eq!(
            sql_type(WarehouseDialect::Snowflake, &SkipprDataType::Timestamp).unwrap(),
            "TIMESTAMP_NTZ"
        );
    }

    #[test]
    fn warehouse_sql_type_fails_closed_on_unmapped_dest_types() {
        for ty in [
            SkipprDataType::Decimal,
            SkipprDataType::Time,
            SkipprDataType::Uuid,
            SkipprDataType::Json,
            SkipprDataType::Binary,
            SkipprDataType::Fixed,
            SkipprDataType::Record,
            SkipprDataType::Map,
            SkipprDataType::Array,
            SkipprDataType::Unknown,
            SkipprDataType::Null,
        ] {
            for dialect in ALL {
                let err = sql_type(dialect, &ty).unwrap_err();
                assert!(
                    err.to_string().contains("no SQL type"),
                    "{dialect:?} {ty:?} {err}"
                );
            }
        }
    }

    #[test]
    fn installed_dest_schema_refuses_an_older_snapshot() {
        let mut installed = InstalledDestSchema::default();
        let first = output_with(&[("region", 1, SkipprDataType::String)]);
        installed.install_state(4, &BTreeMap::from([("trips".into(), first.clone())]));
        assert_eq!(installed.get("trips").unwrap().1.fields.len(), 1);
        let older = output_with(&[]);
        installed.install_state(3, &BTreeMap::from([("trips".into(), older)]));
        assert_eq!(
            installed.get("trips").unwrap().0,
            4,
            "stale InstallSchemaState must not rewind dest metadata"
        );
        let newer = output_with(&[
            ("region", 1, SkipprDataType::String),
            ("fare", 2, SkipprDataType::Double),
        ]);
        installed.install_snapshot(&RuntimeSchemaState {
            version: 5,
            namespaces: BTreeMap::from([("trips".into(), newer)]),
            namespace_versions: BTreeMap::from([("trips".into(), 5)]),
        });
        assert_eq!(installed.get("trips").unwrap().0, 5);
        assert_eq!(installed.get("trips").unwrap().1.fields.len(), 2);
    }

    #[test]
    fn installed_dest_schema_delta_merges_only_newer_namespaces() {
        let initial = output_with(&[("id", 1, SkipprDataType::Long)]);
        let changed = output_with(&[]);
        let mut installed = InstalledDestSchema::default();
        assert!(installed.install_snapshot(&RuntimeSchemaState {
            version: 7,
            namespaces: BTreeMap::from([("events".into(), initial.clone())]),
            namespace_versions: BTreeMap::from([("events".into(), 3)]),
        }));
        let changed_namespaces = installed.install_delta(&SchemaDelta {
            version: 99,
            namespaces: BTreeMap::from([
                (
                    "events".into(),
                    crate::runtime_plugins::protocol::SchemaNamespaceDelta {
                        version: 3,
                        metadata: changed.clone(),
                    },
                ),
                (
                    "users".into(),
                    crate::runtime_plugins::protocol::SchemaNamespaceDelta {
                        version: 1,
                        metadata: changed,
                    },
                ),
            ]),
        });
        assert_eq!(changed_namespaces, vec!["users".to_string()]);
        assert_eq!(installed.get("events").unwrap().1, initial);
        assert_eq!(installed.namespace_versions()["events"], 3);
        assert_eq!(installed.namespace_versions()["users"], 1);
        assert_eq!(installed.version(), 99);
    }

    #[tokio::test]
    async fn dest_publish_cache_hit_skips_repeat_at_same_or_older_version() {
        let cache = DestPublishCache::default();
        let ok = || async { Ok(()) };
        cache.publish("orders", 2, ok).await.unwrap();
        assert!(cache.hit("orders", 1));
        assert!(cache.hit("orders", 2));
        assert!(!cache.hit("orders", 3));
        assert!(!cache.hit("lines", 2));
    }

    #[tokio::test]
    async fn second_publish_at_the_same_version_does_not_create() {
        let exec = FakeExec::new(Vec::new(), None);
        let cache = DestPublishCache::default();
        let metadata = output_with(&[("region", 1, SkipprDataType::String)]);
        publish_warehouse_table(
            &cache,
            &exec,
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            "trips",
            4,
            &metadata,
        )
        .await
        .unwrap();
        let first = exec.ddl.lock().unwrap().clone();
        assert!(
            first.iter().any(|sql| sql.contains("CREATE TABLE")),
            "{first:?}"
        );
        exec.ddl.lock().unwrap().clear();
        publish_warehouse_table(
            &cache,
            &exec,
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            "trips",
            4,
            &metadata,
        )
        .await
        .unwrap();
        assert!(
            exec.ddl.lock().unwrap().is_empty(),
            "same version must not execute DDL"
        );
    }

    #[tokio::test]
    async fn publish_fails_closed_when_ddl_leaves_published_columns_missing() {
        let exec = FakeExec {
            frozen: true,
            ..FakeExec::new(cols(&["id"]), None)
        };
        let cache = DestPublishCache::default();
        let metadata = output_with(&[
            ("id", 1, SkipprDataType::Long),
            ("region", 2, SkipprDataType::String),
        ]);
        let err = publish_warehouse_table(
            &cache,
            &exec,
            WarehouseDialect::Postgres,
            "\"public\".\"trips\"",
            "trips",
            4,
            &metadata,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("missing published column 'region'"),
            "{err}"
        );
        assert!(
            !cache.hit("trips", 4),
            "a failed publish must not be cached"
        );
    }

    #[tokio::test]
    async fn concurrent_publishes_of_one_namespace_run_once() {
        let cache = DestPublishCache::default();
        let ran = std::sync::atomic::AtomicUsize::new(0);
        let publish = || async {
            ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::task::yield_now().await;
            Ok(())
        };
        let (left, right) = tokio::join!(
            cache.publish("orders", 3, publish),
            cache.publish("orders", 3, publish)
        );
        left.unwrap();
        right.unwrap();
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn arrow(names: &[&str]) -> ArrowSchema {
        ArrowSchema::new(
            names
                .iter()
                .map(|name| {
                    datafusion::arrow::datatypes::Field::new(
                        *name,
                        datafusion::arrow::datatypes::DataType::Utf8,
                        true,
                    )
                })
                .collect::<Vec<_>>(),
        )
    }

    fn installed_with(metadata: OutputMetadata) -> InstalledDestSchema {
        let mut installed = InstalledDestSchema::default();
        installed.install_state(1, &BTreeMap::from([("trips".into(), metadata)]));
        installed
    }

    #[test]
    fn batch_column_absent_from_metadata_fails_closed() {
        let installed = installed_with(output_with(&[("region", 1, SkipprDataType::String)]));
        let err = installed
            .warehouse_batch_columns(
                WarehouseDialect::Postgres,
                "trips",
                &arrow(&["region", "fare"]),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("fare") && err.to_string().contains("published schema"),
            "{err}"
        );
        let columns = installed
            .warehouse_batch_columns(WarehouseDialect::Snowflake, "trips", &arrow(&["REGION"]))
            .unwrap();
        assert_eq!(columns.columns(), [("REGION".to_string(), "VARCHAR")]);
        let metadata = installed.metadata("trips").unwrap();
        reject_unpublished_batch_columns(metadata, &arrow(&["region", EMPTY_STRUCT_MARKER_FIELD]))
            .unwrap();
        let missing =
            reject_unpublished_batch_columns(metadata, &arrow(&["region", "fare"])).unwrap_err();
        assert!(missing.to_string().contains("fare"), "{missing}");
        let mut nested = OutputMetadata::new();
        let mut record = OutputMetadata::new();
        record.out_field_name = "attrs".into();
        record.determined_type = SkipprDataType::Record;
        let mut inner = OutputMetadata::new();
        inner.out_field_name = "k".into();
        inner.determined_type = SkipprDataType::String;
        record.fields.insert("k".into(), inner);
        nested.fields.insert("attrs".into(), record);
        reject_unpublished_batch_columns(&nested, &arrow(&["attrs"])).unwrap();
    }

    #[test]
    fn business_keys_resolve_with_the_batch_column_fold() {
        let installed = installed_with(output_with(&[
            ("OrderId", 1, SkipprDataType::Long),
            ("region", 2, SkipprDataType::String),
        ]));
        let batch = arrow(&["OrderId", "region"]);
        for (dialect, want) in [
            (WarehouseDialect::Redshift, ("orderid", "BIGINT")),
            (WarehouseDialect::Postgres, ("orderid", "BIGINT")),
            (WarehouseDialect::Snowflake, ("ORDERID", "NUMBER")),
            (WarehouseDialect::BigQuery, ("orderid", "INT64")),
        ] {
            let columns = installed
                .warehouse_batch_columns(dialect, "trips", &batch)
                .unwrap();
            assert_eq!(
                columns.key_types(&["OrderId".to_string()]).unwrap(),
                vec![(want.0.to_string(), want.1)],
                "{dialect:?}"
            );
            let err = columns.key_types(&["missing".to_string()]).unwrap_err();
            assert!(err.to_string().contains("business key 'missing'"), "{err}");
        }
    }

    #[tokio::test]
    async fn failed_publish_is_not_cached() {
        let cache = DestPublishCache::default();
        let err = cache
            .publish("orders", 1, || async {
                Err(io::Error::other("ddl failed"))
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ddl failed"), "{err}");
        let ran = std::sync::atomic::AtomicUsize::new(0);
        for _ in 0..2 {
            cache
                .publish("orders", 1, || async {
                    ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
                .await
                .unwrap();
        }
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn bigquery_identifiers_escape_backticks_with_backslash() {
        assert_eq!(WarehouseDialect::BigQuery.quote_ident("A`b"), "`a\\`b`");
        assert_eq!(quote(WarehouseDialect::Postgres, "A\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn published_columns_that_fold_together_fail_closed() {
        let metadata = output_with(&[
            ("Id", 1, SkipprDataType::Long),
            ("id", 2, SkipprDataType::Long),
        ]);
        assert_eq!(
            published_columns_folded(&metadata, IdentCase::Exact)
                .unwrap()
                .len(),
            2
        );
        let err = published_columns_folded(&metadata, IdentCase::Lower).unwrap_err();
        assert!(err.to_string().contains("fold to the same"), "{err}");
    }

    #[test]
    fn missing_published_columns_compares_under_the_dest_fold() {
        let metadata = output_with(&[
            ("id", 1, SkipprDataType::Long),
            ("Region", 2, SkipprDataType::String),
        ]);
        let columns = published_columns(&metadata).unwrap();
        let names = |case| {
            missing_published_columns(&columns, &cols(&["ID"]), case)
                .into_iter()
                .map(|column| column.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(IdentCase::Upper), vec!["Region"]);
        assert_eq!(names(IdentCase::Exact), vec!["id", "Region"]);
    }

    #[test]
    fn append_write_paths_have_no_create_or_add_column() {
        for (label, src) in [
            (
                "postgres",
                include_str!("../../plugins/data_sink/postgres/src/postgres.rs"),
            ),
            (
                "snowflake",
                include_str!("../../plugins/data_sink/snowflake/src/snowflake.rs"),
            ),
            (
                "bigquery",
                include_str!("../../plugins/data_sink/bigquery/src/bigquery.rs"),
            ),
            (
                "redshift",
                include_str!("../../plugins/data_sink/redshift/src/redshift.rs"),
            ),
            (
                "motherduck",
                include_str!("../../plugins/data_sink/motherduck/src/motherduck.rs"),
            ),
            (
                "clickhouse",
                include_str!("../../plugins/data_sink/clickhouse/src/clickhouse.rs"),
            ),
            (
                "databricks",
                include_str!("../../plugins/data_sink/databricks/src/databricks.rs"),
            ),
            (
                "synapse",
                include_str!("../../plugins/data_sink/synapse/src/synapse.rs"),
            ),
            (
                "athena",
                include_str!("../../plugins/data_sink/athena/src/athena.rs"),
            ),
        ] {
            assert!(
                !src.contains("arrow_type_to_") && !src.contains("arrow_to_synapse_type"),
                "{label} still maps an Arrow batch to dest DDL"
            );
            assert!(
                !src.contains("col_defs_for_arrow_schema")
                    && !src.contains("col_defs_for_namespace"),
                "{label} still builds column defs from the write batch"
            );
            assert!(
                !src.contains("async fn ensure_table"),
                "{label} still has write-path ensure_table"
            );
            let bodies = write_path_bodies(src);
            if label != "athena" {
                assert!(!bodies.is_empty(), "{label} has no append write path");
            }
            for body in bodies {
                assert!(
                    !body.contains("CREATE TABLE") && !body.contains("ADD COLUMN"),
                    "{label} write path still publishes DDL:\n{body}"
                );
                assert!(
                    !body.contains("ensure_table"),
                    "{label} write path still calls ensure_table"
                );
            }
        }
    }

    fn write_path_bodies(src: &str) -> Vec<String> {
        let lines: Vec<&str> = src.lines().collect();
        let mut bodies = Vec::new();
        let mut index = 0;
        while index < lines.len() {
            if lines[index].contains("fn inner_sync") || lines[index].contains("fn sync_append") {
                let start = index;
                index += 1;
                while index < lines.len() && !next_impl_item(lines[index]) {
                    index += 1;
                }
                bodies.push(lines[start..index].join("\n"));
            } else {
                index += 1;
            }
        }
        bodies
    }

    fn next_impl_item(line: &str) -> bool {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        indent == 4
            && (trimmed.starts_with("async fn ")
                || trimmed.starts_with("fn ")
                || trimmed.starts_with("pub async fn ")
                || trimmed.starts_with("pub fn "))
    }
}
