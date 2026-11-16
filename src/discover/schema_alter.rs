//! SQL ALTER is the table schema contract.
//!
//! [`SchemaAlterOp`] is SQL as written and lives only until [`canonicalize`].
//! [`CanonicalAlter`] is the only resolved alter: typed output names, field
//! ids, and pre-alter types. [`apply`] is the only metadata fold;
//! [`apply_alter`] / [`apply_log`] are the only batch rewrite. Source
//! [`Metadata`] is never mutated by this module.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, RecordBatch, StructArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Field, Fields, Schema};
use arrow::error::ArrowError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::converters::skippr_arrow::convert_skippr_type_to_arrow_data_type;
use crate::discover::{Metadata, SkipprDataType};
pub use crate::migration::journal::{MigrationDirection, MigrationId};
use crate::plugins::alter_ddl::WarehouseDialect;
use crate::plugins::AlterSupport;

const FIELD_ID_KEYS: [&str; 2] = ["PARQUET:field_id", "iceberg.field-id"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FlattenMode {
    Nested,
    Flat,
}

impl FlattenMode {
    pub fn from_flattened(flattened: bool) -> Self {
        if flattened {
            Self::Flat
        } else {
            Self::Nested
        }
    }
}

impl std::fmt::Display for FlattenMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Nested => "nested",
            Self::Flat => "flattened",
        })
    }
}

/// Sole column-name SoT. Nested names are struct paths joined with `.`. Flat
/// names are one top-level column whose metadata path was joined with `_`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "OutputNameRepr", into = "OutputNameRepr")]
pub struct OutputName(Name);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Name {
    Nested {
        segments: Vec<String>,
        joined: String,
    },
    Flat(String),
}

#[derive(Serialize, Deserialize)]
struct OutputNameRepr {
    mode: FlattenMode,
    segments: Vec<String>,
}

impl TryFrom<OutputNameRepr> for OutputName {
    type Error = SchemaAlterError;

    fn try_from(repr: OutputNameRepr) -> Result<Self, Self::Error> {
        if repr.segments.is_empty() || repr.segments.iter().any(String::is_empty) {
            return Err(SchemaAlterError::EmptyName);
        }
        match repr.mode {
            FlattenMode::Nested => Ok(Self::nested(repr.segments)),
            FlattenMode::Flat => match <[String; 1]>::try_from(repr.segments) {
                Ok([column]) => Ok(Self(Name::Flat(column))),
                Err(segments) => Err(SchemaAlterError::SqlFormMismatch {
                    sql: segments.join("."),
                    mode: FlattenMode::Flat,
                }),
            },
        }
    }
}

impl From<OutputName> for OutputNameRepr {
    fn from(name: OutputName) -> Self {
        match name.0 {
            Name::Nested { segments, .. } => Self {
                mode: FlattenMode::Nested,
                segments,
            },
            Name::Flat(column) => Self {
                mode: FlattenMode::Flat,
                segments: vec![column],
            },
        }
    }
}

impl OutputName {
    fn nested(segments: Vec<String>) -> Self {
        let joined = segments.join(".");
        Self(Name::Nested { segments, joined })
    }

    /// Only constructor from metadata paths. Nested joins `.`, Flat joins `_`.
    pub fn from_segments(mode: FlattenMode, segments: &[&str]) -> Self {
        let segments: Vec<String> = segments
            .iter()
            .filter(|segment| !segment.is_empty())
            .map(|segment| segment.to_string())
            .collect();
        match mode {
            FlattenMode::Nested => Self::nested(segments),
            FlattenMode::Flat => Self(Name::Flat(segments.join("_"))),
        }
    }

    /// SQL column reference as written. Flattened pipelines reject dots;
    /// nested pipelines never split `_`.
    pub fn parse_sql(mode: FlattenMode, sql: &str) -> Result<Self, SchemaAlterError> {
        let sql = sql.trim();
        if sql.is_empty() {
            return Err(SchemaAlterError::EmptyName);
        }
        match mode {
            FlattenMode::Flat if sql.contains('.') => Err(SchemaAlterError::SqlFormMismatch {
                sql: sql.to_string(),
                mode,
            }),
            FlattenMode::Flat => Ok(Self(Name::Flat(sql.to_string()))),
            FlattenMode::Nested => {
                let segments: Vec<String> = sql.split('.').map(str::to_string).collect();
                if segments.iter().any(String::is_empty) {
                    return Err(SchemaAlterError::EmptyName);
                }
                Ok(Self::nested(segments))
            }
        }
    }

    pub fn mode(&self) -> FlattenMode {
        match self.0 {
            Name::Nested { .. } => FlattenMode::Nested,
            Name::Flat(_) => FlattenMode::Flat,
        }
    }

    /// Arrow / Iceberg / warehouse / SQL column name.
    pub fn column(&self) -> &str {
        match &self.0 {
            Name::Nested { joined, .. } => joined,
            Name::Flat(column) => column,
        }
    }

    /// Field name inside its parent. A flat column is its own leaf.
    pub fn leaf(&self) -> &str {
        match &self.0 {
            Name::Nested { segments, .. } => segments.last().map_or("", String::as_str),
            Name::Flat(column) => column,
        }
    }

    /// Enclosing struct path. Flat columns are always top-level.
    pub fn parent(&self) -> &[String] {
        match &self.0 {
            Name::Nested { segments, .. } => &segments[..segments.len().saturating_sub(1)],
            Name::Flat(_) => &[],
        }
    }

    /// Same parent, new leaf. `leaf` is one identifier.
    pub fn with_leaf(&self, leaf: &str) -> Result<Self, SchemaAlterError> {
        let leaf = leaf.trim();
        if leaf.is_empty() {
            return Err(SchemaAlterError::EmptyName);
        }
        if leaf.contains('.') {
            return Err(SchemaAlterError::RenameTargetNotLeaf(leaf.to_string()));
        }
        Ok(match &self.0 {
            Name::Nested { segments, .. } => {
                let mut segments = segments.clone();
                segments.pop();
                segments.push(leaf.to_string());
                Self::nested(segments)
            }
            Name::Flat(_) => Self(Name::Flat(leaf.to_string())),
        })
    }
}

impl std::fmt::Display for OutputName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.column())
    }
}

/// SQL as written. Only [`canonicalize`] reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemaAlterOp {
    Rename { from: String, to: String },
    Merge { src: String, dst: String },
    Drop { column: String },
    Promote { column: String, to: SkipprDataType },
}

/// One output column resolved at canonicalize: name, skipprd field id, and
/// its type before the alter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlterColumn {
    pub name: OutputName,
    pub id: i32,
    pub ty: SkipprDataType,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CanonicalAlter {
    /// `to_leaf` replaces the last segment; the column keeps its parent.
    Rename {
        column: AlterColumn,
        to_leaf: String,
    },
    Merge {
        src: AlterColumn,
        dst: AlterColumn,
    },
    Drop {
        column: AlterColumn,
    },
    Promote {
        column: AlterColumn,
        to: SkipprDataType,
    },
}

impl CanonicalAlter {
    /// The column this alter rewrites (MERGE: the source that disappears).
    pub fn column(&self) -> &AlterColumn {
        match self {
            Self::Rename { column, .. } | Self::Drop { column } | Self::Promote { column, .. } => {
                column
            }
            Self::Merge { src, .. } => src,
        }
    }

    pub fn mode(&self) -> FlattenMode {
        self.column().name.mode()
    }

    pub fn rename_target(&self) -> Result<Option<OutputName>, SchemaAlterError> {
        match self {
            Self::Rename { column, to_leaf } => column.name.with_leaf(to_leaf).map(Some),
            _ => Ok(None),
        }
    }

    fn columns(&self) -> impl Iterator<Item = &AlterColumn> {
        let (first, second) = match self {
            Self::Merge { src, dst } => (src, Some(dst)),
            _ => (self.column(), None),
        };
        std::iter::once(first).chain(second)
    }

    /// Iceberg readers project by field id, so rename, drop, and v2-legal
    /// promotes are metadata-only. MERGE moves data and date→timestamp is not
    /// a v2 promotion: both rewrite data files.
    pub fn rewrites_data(&self) -> bool {
        match self {
            Self::Merge { .. } => true,
            Self::Promote { column, to } => {
                column.ty == SkipprDataType::Date && *to == SkipprDataType::Timestamp
            }
            Self::Rename { .. } | Self::Drop { .. } => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlterOrigin {
    pub migration: MigrationId,
    pub direction: MigrationDirection,
}

/// One entry of a namespace's output-shape log. `seq` is its position in that
/// log and is the destination idempotency key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedAlter {
    pub origin: AlterOrigin,
    pub seq: u64,
    pub alter: CanonicalAlter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastRule {
    Identity,
    ArrowCast,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastPurpose {
    Migration,
    Merge,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DestRejection {
    #[error("this destination does not support migrations")]
    NotTable,
    #[error("warehouse columns are top-level; '{0}' is nested")]
    NestedColumn(OutputName),
    #[error("{0:?} cannot ALTER date to timestamp")]
    DateToTimestamp(WarehouseDialect),
    #[error("Redshift only changes VARCHAR length; use MERGE COLUMN")]
    RedshiftTypeChange,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SchemaAlterError {
    #[error("column name is empty")]
    EmptyName,
    #[error("column '{0}' not found")]
    ColumnNotFound(String),
    #[error("column '{0}' already exists")]
    ColumnExists(String),
    #[error("column '{0}' matches more than one flattened field")]
    AmbiguousName(String),
    #[error("schema '{0}' not found")]
    NamespaceNotFound(String),
    #[error(
        "cannot promote {from} to {to}; legal promotions are byte/short/integer→long, byte/short→integer, float→double, timestamp_milli→timestamp, date→timestamp; widen to string with MERGE COLUMN"
    )]
    IllegalPromote {
        from: SkipprDataType,
        to: SkipprDataType,
    },
    #[error("SQL '{sql}' does not match the {mode} output schema (flattened pipelines use underscore names like detail_region)")]
    SqlFormMismatch { sql: String, mode: FlattenMode },
    #[error("RENAME COLUMN target '{0}' must be one identifier; the column keeps its parent")]
    RenameTargetNotLeaf(String),
    #[error("MERGE COLUMN source and destination must differ")]
    MergeSameColumn,
    #[error("MERGE COLUMN destination '{0}' is absent and in a different parent")]
    MergeTargetMissing(String),
    #[error("ALTER of a field inside a List or Map is not supported")]
    NestedCollection,
    #[error("expected a struct at '{0}'")]
    NotStruct(String),
    #[error("field id {id} does not match column '{name}'")]
    IdNameMismatch { id: i32, name: String },
    #[error("alter log was recorded for {recorded} output but the pipeline is now {current}")]
    FlattenModeChanged {
        recorded: FlattenMode,
        current: FlattenMode,
    },
    #[error("column '{0}' is referenced by pipeline config; update the config first")]
    ColumnInUse(String),
    #[error("record carries both '{output}' and its source key '{source_key}'")]
    IngestKeyCollision { output: String, source_key: String },
    #[error("destination does not accept this ALTER: {0}")]
    DestRejects(DestRejection),
    #[error("arrow: {0}")]
    Arrow(String),
    #[error("iceberg: {0}")]
    Iceberg(String),
}

impl From<ArrowError> for SchemaAlterError {
    fn from(err: ArrowError) -> Self {
        SchemaAlterError::Arrow(err.to_string())
    }
}

pub fn is_iceberg_legal_promote(from: &SkipprDataType, to: &SkipprDataType) -> bool {
    use SkipprDataType::*;
    from == to
        || matches!(
            (from, to),
            (Byte | Short | Integer, Long)
                | (Byte | Short, Integer)
                | (Float, Double)
                | (TimestampMilli, Timestamp)
                | (Date, Timestamp)
        )
}

pub fn cast_rule(
    from: &SkipprDataType,
    to: &SkipprDataType,
    purpose: CastPurpose,
) -> Result<CastRule, SchemaAlterError> {
    use SkipprDataType::*;
    if from == to {
        return Ok(CastRule::Identity);
    }
    let legal = is_iceberg_legal_promote(from, to)
        || (purpose == CastPurpose::Merge
            && matches!((from, to), (Byte | Short | Integer | Long, String)));
    if legal {
        Ok(CastRule::ArrowCast)
    } else {
        Err(SchemaAlterError::IllegalPromote {
            from: from.clone(),
            to: to.clone(),
        })
    }
}

/// Destination capability check. Runs before any WAL or destination I/O.
pub fn dest_accepts(support: AlterSupport, alter: &CanonicalAlter) -> Result<(), SchemaAlterError> {
    let reject = |why| Err(SchemaAlterError::DestRejects(why));
    let dialect = match support {
        AlterSupport::Unsupported => return reject(DestRejection::NotTable),
        AlterSupport::Iceberg => return Ok(()),
        AlterSupport::Warehouse(dialect) => dialect,
    };
    if let Some(nested) = alter
        .columns()
        .find(|column| !column.name.parent().is_empty())
    {
        return reject(DestRejection::NestedColumn(nested.name.clone()));
    }
    if let CanonicalAlter::Promote { column, to } = alter {
        if dialect == WarehouseDialect::Redshift {
            return reject(DestRejection::RedshiftTypeChange);
        }
        if column.ty == SkipprDataType::Date
            && *to == SkipprDataType::Timestamp
            && matches!(
                dialect,
                WarehouseDialect::Snowflake | WarehouseDialect::BigQuery
            )
        {
            return reject(DestRejection::DateToTimestamp(dialect));
        }
    }
    Ok(())
}

pub fn flatten_child_path(field_path: &str, child_out: &str) -> String {
    OutputName::from_segments(FlattenMode::Flat, &[field_path, child_out])
        .column()
        .to_string()
}

/// Resolve SQL against the current output schema (`PipelineMetadata::output`
/// plus earlier statements of the same migration). `in_use` lists columns the
/// pipeline config references; renaming, dropping, or merging them away fails.
pub fn canonicalize(
    mode: FlattenMode,
    output: &Metadata,
    op: &SchemaAlterOp,
    in_use: &[OutputName],
) -> Result<CanonicalAlter, SchemaAlterError> {
    let resolve = |sql: &str| -> Result<AlterColumn, SchemaAlterError> {
        let name = OutputName::parse_sql(mode, sql)?;
        let path = locate(output, &name)?
            .ok_or_else(|| SchemaAlterError::ColumnNotFound(name.column().to_string()))?;
        let field = field_at_path(output, &path)
            .ok_or_else(|| SchemaAlterError::ColumnNotFound(name.column().to_string()))?;
        Ok(AlterColumn {
            name,
            id: field.field_id,
            ty: field.determined_type.clone(),
        })
    };
    let free = |column: &AlterColumn| {
        if in_use.contains(&column.name) {
            Err(SchemaAlterError::ColumnInUse(
                column.name.column().to_string(),
            ))
        } else {
            Ok(())
        }
    };
    match op {
        SchemaAlterOp::Rename { from, to } => {
            let column = resolve(from)?;
            let target = column.name.with_leaf(to)?;
            if target != column.name && locate(output, &target)?.is_some() {
                return Err(SchemaAlterError::ColumnExists(target.column().to_string()));
            }
            free(&column)?;
            Ok(CanonicalAlter::Rename {
                to_leaf: target.leaf().to_string(),
                column,
            })
        }
        SchemaAlterOp::Merge { src, dst } => {
            let src = resolve(src)?;
            let dst = resolve(dst)?;
            if src.id == dst.id || src.name == dst.name {
                return Err(SchemaAlterError::MergeSameColumn);
            }
            cast_rule(&src.ty, &dst.ty, CastPurpose::Merge)?;
            free(&src)?;
            Ok(CanonicalAlter::Merge { src, dst })
        }
        SchemaAlterOp::Drop { column } => {
            let column = resolve(column)?;
            free(&column)?;
            Ok(CanonicalAlter::Drop { column })
        }
        SchemaAlterOp::Promote { column, to } => {
            let column = resolve(column)?;
            cast_rule(&column.ty, to, CastPurpose::Migration)?;
            Ok(CanonicalAlter::Promote {
                column,
                to: to.clone(),
            })
        }
    }
}

/// Pure metadata fold of one alter onto output metadata.
pub fn apply(mut metadata: Metadata, alter: &CanonicalAlter) -> Result<Metadata, SchemaAlterError> {
    match alter {
        CanonicalAlter::Rename { column, to_leaf } => {
            let target = column.name.with_leaf(to_leaf)?;
            if target != column.name && locate(&metadata, &target)?.is_some() {
                return Err(SchemaAlterError::ColumnExists(target.column().to_string()));
            }
            let path = checked_path(&metadata, column)?;
            let mut field = take_at_path(&mut metadata, &path)?;
            field.out_field_name = to_leaf.clone();
            let parent = match column.name.mode() {
                FlattenMode::Nested => &path[..path.len() - 1],
                FlattenMode::Flat => &[],
            };
            field_at_path_mut(&mut metadata, parent)
                .ok_or_else(|| SchemaAlterError::NotStruct(parent.join(".")))?
                .fields
                .insert(to_leaf.clone(), field);
        }
        CanonicalAlter::Merge { src, dst } => {
            checked_path(&metadata, dst)?;
            let path = checked_path(&metadata, src)?;
            take_at_path(&mut metadata, &path)?;
        }
        CanonicalAlter::Drop { column } => {
            let path = checked_path(&metadata, column)?;
            take_at_path(&mut metadata, &path)?;
        }
        CanonicalAlter::Promote { column, to } => {
            let path = checked_path(&metadata, column)?;
            let field = field_at_path_mut(&mut metadata, &path)
                .ok_or_else(|| SchemaAlterError::ColumnNotFound(column.name.to_string()))?;
            cast_rule(&field.determined_type, to, CastPurpose::Migration)?;
            field.determined_type = to.clone();
        }
    }
    Ok(metadata)
}

fn segment_of<'a>(key: &'a str, field: &'a Metadata) -> &'a str {
    if field.out_field_name.is_empty() {
        key
    } else {
        &field.out_field_name
    }
}

/// Source-key path of the field whose output name is `want`. Flat names match
/// leaves only; more than one match fails closed.
fn locate(root: &Metadata, want: &OutputName) -> Result<Option<Vec<String>>, SchemaAlterError> {
    fn walk(
        node: &Metadata,
        want: &OutputName,
        segments: &mut Vec<String>,
        keys: &mut Vec<String>,
        in_collection: bool,
        hits: &mut Vec<(Vec<String>, bool)>,
    ) {
        for (key, child) in node.fields.iter() {
            segments.push(segment_of(key, child).to_string());
            keys.push(key.clone());
            let is_leaf = child.fields.is_empty();
            let matchable = want.mode() == FlattenMode::Nested || is_leaf;
            let name = OutputName::from_segments(
                want.mode(),
                &segments.iter().map(String::as_str).collect::<Vec<_>>(),
            );
            if matchable && &name == want {
                hits.push((keys.clone(), in_collection));
            }
            let child_in_collection =
                in_collection || child.determined_type != SkipprDataType::Record;
            walk(child, want, segments, keys, child_in_collection, hits);
            segments.pop();
            keys.pop();
        }
    }
    let mut hits = Vec::new();
    walk(
        root,
        want,
        &mut Vec::new(),
        &mut Vec::new(),
        false,
        &mut hits,
    );
    match hits.as_slice() {
        [] => Ok(None),
        [(_, true)] => Err(SchemaAlterError::NestedCollection),
        [(path, false)] => Ok(Some(path.clone())),
        _ => Err(SchemaAlterError::AmbiguousName(want.column().to_string())),
    }
}

fn checked_path(
    metadata: &Metadata,
    column: &AlterColumn,
) -> Result<Vec<String>, SchemaAlterError> {
    let path = locate(metadata, &column.name)?
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(column.name.to_string()))?;
    let field = field_at_path(metadata, &path)
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(column.name.to_string()))?;
    if field.field_id != column.id {
        return Err(SchemaAlterError::IdNameMismatch {
            id: column.id,
            name: column.name.to_string(),
        });
    }
    Ok(path)
}

fn field_at_path<'a>(root: &'a Metadata, path: &[String]) -> Option<&'a Metadata> {
    path.iter().try_fold(root, |node, key| node.fields.get(key))
}

fn field_at_path_mut<'a>(root: &'a mut Metadata, path: &[String]) -> Option<&'a mut Metadata> {
    path.iter()
        .try_fold(root, |node, key| node.fields.get_mut(key))
}

fn take_at_path(root: &mut Metadata, path: &[String]) -> Result<Metadata, SchemaAlterError> {
    let (leaf, parent) = path.split_last().ok_or(SchemaAlterError::EmptyName)?;
    field_at_path_mut(root, parent)
        .and_then(|parent| parent.fields.remove(leaf))
        .ok_or_else(|| SchemaAlterError::ColumnNotFound(path.join(".")))
}

/// Maps renamed top-level JSON keys back to their source key so a source that
/// adopted the new name does not discover a sibling column. Built once per
/// namespace metadata snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IngestKeys(HashMap<String, String>);

impl IngestKeys {
    pub fn new(source: &Metadata, output: &Metadata) -> Self {
        let source_by_id: HashMap<i32, &String> = source
            .fields
            .iter()
            .filter(|(_, field)| field.field_id != 0)
            .map(|(key, field)| (field.field_id, key))
            .collect();
        Self(
            output
                .fields
                .iter()
                .filter(|(_, field)| field.field_id != 0)
                .filter_map(|(key, field)| {
                    let name = segment_of(key, field);
                    let source_key = *source_by_id.get(&field.field_id)?;
                    (name != source_key && !source.fields.contains_key(name))
                        .then(|| (name.to_string(), source_key.clone()))
                })
                .collect(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Move renamed keys to their source key. A record carrying both fails.
    pub fn rewrite(
        &self,
        record: &mut serde_json::Map<String, Value>,
    ) -> Result<(), SchemaAlterError> {
        for (output, source) in &self.0 {
            if let Some(value) = record.remove(output) {
                if record.contains_key(source) {
                    return Err(SchemaAlterError::IngestKeyCollision {
                        output: output.clone(),
                        source_key: source.clone(),
                    });
                }
                record.insert(source.clone(), value);
            }
        }
        Ok(())
    }
}

/// Indices from the batch root through Struct fields to the leaf.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FieldPath(Vec<usize>);

/// Rewrite one batch with one committed alter. Binds by output name; a field
/// that carries a field id must carry this column's id. A column absent from
/// the batch (older data, or already rewritten on resume) is a no-op.
pub fn apply_alter(
    batch: RecordBatch,
    alter: &CanonicalAlter,
) -> Result<RecordBatch, SchemaAlterError> {
    let schema = batch.schema();
    match alter {
        CanonicalAlter::Rename { column, to_leaf } => {
            let Some(path) = resolve(&schema, column)? else {
                return Ok(batch);
            };
            let target = column.name.with_leaf(to_leaf)?;
            if resolve_name(&schema, &target)?.is_some() {
                return Err(SchemaAlterError::ColumnExists(target.to_string()));
            }
            rename_at(batch, &path, to_leaf)
        }
        CanonicalAlter::Drop { column } => match resolve(&schema, column)? {
            Some(path) => drop_at(batch, &path),
            None => Ok(batch),
        },
        CanonicalAlter::Promote { column, to } => match resolve(&schema, column)? {
            Some(path) => promote_at(batch, &path, &arrow_type(to)?),
            None => Ok(batch),
        },
        CanonicalAlter::Merge { src, dst } => {
            let Some(src_path) = resolve(&schema, src)? else {
                return Ok(batch);
            };
            match resolve(&schema, dst)? {
                Some(dst_path) => merge_at(
                    batch,
                    &src_path,
                    &dst_path,
                    cast_rule(&src.ty, &dst.ty, CastPurpose::Merge)?,
                ),
                None if src.name.parent() == dst.name.parent() => {
                    let batch = promote_at(batch, &src_path, &arrow_type(&dst.ty)?)?;
                    rename_at(batch, &src_path, dst.name.leaf())
                }
                None => Err(SchemaAlterError::MergeTargetMissing(dst.name.to_string())),
            }
        }
    }
}

/// Rewrite one batch with a namespace log, each alter bound against the
/// previous alter's output.
pub fn apply_log(
    batch: RecordBatch,
    log: &[CommittedAlter],
) -> Result<RecordBatch, SchemaAlterError> {
    log.iter().try_fold(batch, |batch, committed| {
        apply_alter(batch, &committed.alter)
    })
}

/// Stream YAML `rename`: literal top-level names, applied simultaneously.
pub fn rename_columns(
    batch: RecordBatch,
    renames: &[(String, String)],
) -> Result<RecordBatch, SchemaAlterError> {
    if renames.is_empty() {
        return Ok(batch);
    }
    let schema = batch.schema();
    let mut fields: Vec<Field> = schema.fields().iter().map(|f| f.as_ref().clone()).collect();
    for (from, to) in renames {
        let idx = schema
            .index_of(from)
            .map_err(|_| SchemaAlterError::ColumnNotFound(from.clone()))?;
        fields[idx] = fields[idx].clone().with_name(to);
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(dup) = fields.iter().find(|f| !seen.insert(f.name().clone())) {
        return Err(SchemaAlterError::ColumnExists(dup.name().clone()));
    }
    Ok(RecordBatch::try_new(
        Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone())),
        batch.columns().to_vec(),
    )?)
}

/// Stream YAML `cast` of one top-level column; keeps field metadata.
pub fn cast_column(
    batch: RecordBatch,
    column: &str,
    to: &DataType,
) -> Result<RecordBatch, SchemaAlterError> {
    let idx = batch
        .schema()
        .index_of(column)
        .map_err(|_| SchemaAlterError::ColumnNotFound(column.to_string()))?;
    promote_at(batch, &FieldPath(vec![idx]), to)
}

fn arrow_type(ty: &SkipprDataType) -> Result<DataType, SchemaAlterError> {
    Ok(convert_skippr_type_to_arrow_data_type(ty)?)
}

fn resolve(schema: &Schema, column: &AlterColumn) -> Result<Option<FieldPath>, SchemaAlterError> {
    let Some(path) = resolve_name(schema, &column.name)? else {
        return Ok(None);
    };
    match field_id_of(field_at(schema, &path)?) {
        Some(id) if id != column.id => Err(SchemaAlterError::IdNameMismatch {
            id: column.id,
            name: column.name.to_string(),
        }),
        _ => Ok(Some(path)),
    }
}

fn resolve_name(schema: &Schema, name: &OutputName) -> Result<Option<FieldPath>, SchemaAlterError> {
    let segments: Vec<&str> = match &name.0 {
        Name::Flat(column) => vec![column.as_str()],
        Name::Nested { segments, .. } => segments.iter().map(String::as_str).collect(),
    };
    let mut path = Vec::with_capacity(segments.len());
    let mut fields = schema.fields();
    for (depth, segment) in segments.iter().enumerate() {
        let Some(idx) = fields.iter().position(|field| field.name() == segment) else {
            return Ok(None);
        };
        path.push(idx);
        if depth + 1 < segments.len() {
            fields = match fields[idx].data_type() {
                DataType::Struct(children) => children,
                DataType::List(_) | DataType::LargeList(_) | DataType::Map(_, _) => {
                    return Err(SchemaAlterError::NestedCollection)
                }
                _ => return Ok(None),
            };
        }
    }
    Ok(Some(FieldPath(path)))
}

fn field_id_of(field: &Field) -> Option<i32> {
    FIELD_ID_KEYS
        .iter()
        .find_map(|key| field.metadata().get(*key).and_then(|v| v.parse().ok()))
}

fn field_at<'a>(schema: &'a Schema, path: &FieldPath) -> Result<&'a Field, SchemaAlterError> {
    let mut field = schema.field(path.0[0]);
    for idx in &path.0[1..] {
        let DataType::Struct(children) = field.data_type() else {
            return Err(SchemaAlterError::NotStruct(field.name().clone()));
        };
        field = &children[*idx];
    }
    Ok(field)
}

fn array_at(batch: &RecordBatch, path: &FieldPath) -> Result<ArrayRef, SchemaAlterError> {
    let mut array = batch.column(path.0[0]).clone();
    for idx in &path.0[1..] {
        let st = array
            .as_any()
            .downcast_ref::<StructArray>()
            .ok_or_else(|| SchemaAlterError::NotStruct(format!("{:?}", path.0)))?;
        array = st.column(*idx).clone();
    }
    Ok(array)
}

fn rename_at(
    batch: RecordBatch,
    path: &FieldPath,
    to: &str,
) -> Result<RecordBatch, SchemaAlterError> {
    map_children(batch, path, |fields, _, leaf| {
        fields[leaf] = fields[leaf].clone().with_name(to);
        Ok(())
    })
}

fn drop_at(batch: RecordBatch, path: &FieldPath) -> Result<RecordBatch, SchemaAlterError> {
    map_children(batch, path, |fields, columns, leaf| {
        fields.remove(leaf);
        columns.remove(leaf);
        Ok(())
    })
}

fn promote_at(
    batch: RecordBatch,
    path: &FieldPath,
    to: &DataType,
) -> Result<RecordBatch, SchemaAlterError> {
    map_children(batch, path, |fields, columns, leaf| {
        columns[leaf] = cast(&columns[leaf], to)?;
        fields[leaf] = fields[leaf].clone().with_data_type(to.clone());
        Ok(())
    })
}

fn merge_at(
    batch: RecordBatch,
    src: &FieldPath,
    dst: &FieldPath,
    rule: CastRule,
) -> Result<RecordBatch, SchemaAlterError> {
    let dst_type = field_at(batch.schema().as_ref(), dst)?.data_type().clone();
    let src_array = array_at(&batch, src)?;
    let src_array = match rule {
        CastRule::Identity => src_array,
        CastRule::ArrowCast => cast(&src_array, &dst_type)?,
    };
    let dst_array = array_at(&batch, dst)?;
    let take_dst = arrow::compute::is_not_null(&dst_array)?;
    let merged = arrow::compute::kernels::zip::zip(&take_dst, &dst_array, &src_array)?;
    let batch = map_children(batch, dst, |_, columns, leaf| {
        columns[leaf] = merged;
        Ok(())
    })?;
    drop_at(batch, src)
}

type Children = (Vec<Field>, Vec<ArrayRef>);

/// Edit the children of the struct (or batch root) that holds the leaf at
/// `path`, rebuilding every enclosing struct. Field metadata is preserved.
fn map_children(
    batch: RecordBatch,
    path: &FieldPath,
    edit: impl FnOnce(&mut Vec<Field>, &mut Vec<ArrayRef>, usize) -> Result<(), SchemaAlterError>,
) -> Result<RecordBatch, SchemaAlterError> {
    fn descend(
        (mut fields, mut columns): Children,
        path: &[usize],
        edit: impl FnOnce(&mut Vec<Field>, &mut Vec<ArrayRef>, usize) -> Result<(), SchemaAlterError>,
    ) -> Result<Children, SchemaAlterError> {
        let (idx, rest) = path.split_first().ok_or(SchemaAlterError::EmptyName)?;
        if rest.is_empty() {
            edit(&mut fields, &mut columns, *idx)?;
            return Ok((fields, columns));
        }
        let st = columns[*idx]
            .as_any()
            .downcast_ref::<StructArray>()
            .ok_or_else(|| SchemaAlterError::NotStruct(fields[*idx].name().clone()))?
            .clone();
        let nulls = st.nulls().cloned();
        let len = st.len();
        let (child_fields, child_columns, _) = st.into_parts();
        let children = (
            child_fields.iter().map(|f| f.as_ref().clone()).collect(),
            child_columns,
        );
        let (child_fields, child_columns) = descend(children, rest, edit)?;
        let child_fields: Fields = child_fields.into();
        let rebuilt = if child_fields.is_empty() {
            StructArray::new_empty_fields(len, nulls)
        } else {
            StructArray::try_new(child_fields.clone(), child_columns, nulls)?
        };
        fields[*idx] = fields[*idx]
            .clone()
            .with_data_type(DataType::Struct(child_fields));
        columns[*idx] = Arc::new(rebuilt);
        Ok((fields, columns))
    }

    let schema = batch.schema();
    let root = (
        schema.fields().iter().map(|f| f.as_ref().clone()).collect(),
        batch.columns().to_vec(),
    );
    let (fields, columns) = descend(root, &path.0, edit)?;
    Ok(RecordBatch::try_new(
        Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone())),
        columns,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::converters::skippr_arrow::convert_skippr_to_arrow;
    use crate::discover::OutputMetadata;
    use arrow::array::{Date32Array, Int64Array, StringArray};
    use proptest::prelude::*;

    fn field(name: &str, ty: SkipprDataType, field_id: i32) -> Metadata {
        let mut m = Metadata::new_with_type(ty, name);
        m.field_id = field_id;
        m
    }

    fn ns(fields: Vec<Metadata>) -> Metadata {
        let mut root = Metadata::new().unwrap();
        for f in fields {
            root.set_field(&f.out_field_name.clone(), f);
        }
        root
    }

    fn record(name: &str, id: i32, children: Vec<Metadata>) -> Metadata {
        let mut parent = field(name, SkipprDataType::Record, id);
        for child in children {
            parent.set_field(&child.out_field_name.clone(), child);
        }
        parent
    }

    fn trips() -> Metadata {
        ns(vec![
            field("region", SkipprDataType::String, 21),
            field("price", SkipprDataType::String, 7),
            field("price_string", SkipprDataType::String, 9),
            field("n", SkipprDataType::Integer, 3),
            field("event_day", SkipprDataType::Date, 11),
            record(
                "detail",
                1,
                vec![
                    field("truck_reg", SkipprDataType::String, 4),
                    field("zone", SkipprDataType::String, 5),
                ],
            ),
        ])
    }

    fn col(mode: FlattenMode, meta: &Metadata, sql: &str) -> AlterColumn {
        match canonicalize(mode, meta, &SchemaAlterOp::Drop { column: sql.into() }, &[]).unwrap() {
            CanonicalAlter::Drop { column } => column,
            other => panic!("{other:?}"),
        }
    }

    fn host_arrow(meta: &Metadata, mode: FlattenMode) -> Arc<Schema> {
        let out = match mode {
            FlattenMode::Flat => OutputMetadata::from_flatterened_metadata(meta),
            FlattenMode::Nested => OutputMetadata::from_metadata(meta),
        };
        Arc::new(convert_skippr_to_arrow(out.fields).unwrap())
    }

    fn names(fields: &Fields, prefix: &str, out: &mut Vec<String>) {
        for f in fields {
            let name = format!("{prefix}{}", f.name());
            if let DataType::Struct(children) = f.data_type() {
                names(children, &format!("{name}."), out);
            }
            out.push(name);
        }
    }

    fn sorted_names(schema: &Schema) -> Vec<String> {
        let mut out = Vec::new();
        names(schema.fields(), "", &mut out);
        out.sort();
        out
    }

    fn id_field(name: &str, dt: DataType, id: i32) -> Field {
        Field::new(name, dt, true).with_metadata(HashMap::from([(
            "PARQUET:field_id".to_string(),
            id.to_string(),
        )]))
    }

    fn one_col(field: Field, array: ArrayRef) -> RecordBatch {
        RecordBatch::try_new(Arc::new(Schema::new(vec![field])), vec![array]).unwrap()
    }

    #[test]
    fn output_name_joins_by_mode() {
        let flat = OutputName::from_segments(FlattenMode::Flat, &["detail", "region"]);
        assert_eq!(flat.column(), "detail_region");
        assert_eq!(flat.leaf(), "detail_region");
        assert!(flat.parent().is_empty());
        let nested = OutputName::from_segments(FlattenMode::Nested, &["detail", "region"]);
        assert_eq!(nested.column(), "detail.region");
        assert_eq!(nested.leaf(), "region");
        assert_eq!(nested.parent(), ["detail".to_string()]);
    }

    #[test]
    fn parse_sql_is_mode_strict() {
        assert!(matches!(
            OutputName::parse_sql(FlattenMode::Flat, "detail.region"),
            Err(SchemaAlterError::SqlFormMismatch { .. })
        ));
        let nested = OutputName::parse_sql(FlattenMode::Nested, "detail_region").unwrap();
        assert!(nested.parent().is_empty());
        assert_eq!(
            OutputName::parse_sql(FlattenMode::Nested, "detail..region"),
            Err(SchemaAlterError::EmptyName)
        );
    }

    #[test]
    fn with_leaf_keeps_parent_and_rejects_paths() {
        let nested = OutputName::parse_sql(FlattenMode::Nested, "detail.region").unwrap();
        assert_eq!(nested.with_leaf("zone").unwrap().column(), "detail.zone");
        assert_eq!(
            nested.with_leaf("other.zone"),
            Err(SchemaAlterError::RenameTargetNotLeaf("other.zone".into()))
        );
    }

    #[test]
    fn output_name_serde_round_trips_and_validates() {
        for name in [
            OutputName::from_segments(FlattenMode::Nested, &["detail", "region"]),
            OutputName::from_segments(FlattenMode::Flat, &["detail", "region"]),
        ] {
            let json = serde_json::to_string(&name).unwrap();
            assert_eq!(serde_json::from_str::<OutputName>(&json).unwrap(), name);
        }
        let bad = r#"{"mode":"Flat","segments":["detail","region"]}"#;
        assert!(serde_json::from_str::<OutputName>(bad).is_err());
    }

    #[test]
    fn canonicalize_carries_name_id_and_source_type() {
        let alter = canonicalize(
            FlattenMode::Flat,
            &trips(),
            &SchemaAlterOp::Rename {
                from: "detail_truck_reg".into(),
                to: "truck_registration".into(),
            },
            &[],
        )
        .unwrap();
        let CanonicalAlter::Rename { column, to_leaf } = &alter else {
            panic!("{alter:?}");
        };
        assert_eq!(column.name.column(), "detail_truck_reg");
        assert_eq!(column.id, 4);
        assert_eq!(column.ty, SkipprDataType::String);
        assert_eq!(to_leaf, "truck_registration");
    }

    #[test]
    fn canonicalize_fails_closed() {
        let meta = trips();
        let cases = [
            (
                FlattenMode::Nested,
                SchemaAlterOp::Drop {
                    column: "detail_truck_reg".into(),
                },
                SchemaAlterError::ColumnNotFound("detail_truck_reg".into()),
            ),
            (
                FlattenMode::Nested,
                SchemaAlterOp::Rename {
                    from: "region".into(),
                    to: "detail.region".into(),
                },
                SchemaAlterError::RenameTargetNotLeaf("detail.region".into()),
            ),
            (
                FlattenMode::Nested,
                SchemaAlterOp::Rename {
                    from: "region".into(),
                    to: "price".into(),
                },
                SchemaAlterError::ColumnExists("price".into()),
            ),
            (
                FlattenMode::Nested,
                SchemaAlterOp::Merge {
                    src: "price".into(),
                    dst: "price".into(),
                },
                SchemaAlterError::MergeSameColumn,
            ),
            (
                FlattenMode::Nested,
                SchemaAlterOp::Merge {
                    src: "price_string".into(),
                    dst: "n".into(),
                },
                SchemaAlterError::IllegalPromote {
                    from: SkipprDataType::String,
                    to: SkipprDataType::Integer,
                },
            ),
            (
                FlattenMode::Flat,
                SchemaAlterOp::Drop {
                    column: "detail".into(),
                },
                SchemaAlterError::ColumnNotFound("detail".into()),
            ),
        ];
        for (mode, op, want) in cases {
            assert_eq!(canonicalize(mode, &meta, &op, &[]), Err(want), "{op:?}");
        }
    }

    #[test]
    fn canonicalize_rejects_config_referenced_columns() {
        let in_use = [OutputName::parse_sql(FlattenMode::Nested, "event_day").unwrap()];
        assert_eq!(
            canonicalize(
                FlattenMode::Nested,
                &trips(),
                &SchemaAlterOp::Drop {
                    column: "event_day".into()
                },
                &in_use,
            ),
            Err(SchemaAlterError::ColumnInUse("event_day".into()))
        );
        canonicalize(
            FlattenMode::Nested,
            &trips(),
            &SchemaAlterOp::Promote {
                column: "event_day".into(),
                to: SkipprDataType::Timestamp,
            },
            &in_use,
        )
        .unwrap();
    }

    #[test]
    fn canonicalize_flat_ambiguous_name_fails_closed() {
        let meta = ns(vec![
            record("a_b", 1, vec![field("c", SkipprDataType::String, 2)]),
            record("a", 3, vec![field("b_c", SkipprDataType::String, 4)]),
        ]);
        assert_eq!(
            canonicalize(
                FlattenMode::Flat,
                &meta,
                &SchemaAlterOp::Drop {
                    column: "a_b_c".into()
                },
                &[],
            ),
            Err(SchemaAlterError::AmbiguousName("a_b_c".into()))
        );
    }

    #[test]
    fn nested_rename_stays_in_parent_and_keeps_id() {
        let meta = trips();
        let alter = canonicalize(
            FlattenMode::Nested,
            &meta,
            &SchemaAlterOp::Rename {
                from: "detail.truck_reg".into(),
                to: "registration".into(),
            },
            &[],
        )
        .unwrap();
        let folded = apply(meta, &alter).unwrap();
        let detail = folded.fields.get("detail").unwrap();
        assert!(detail.fields.get("truck_reg").is_none());
        let renamed = detail.fields.get("registration").unwrap();
        assert_eq!(renamed.out_field_name, "registration");
        assert_eq!(renamed.field_id, 4);
    }

    #[test]
    fn fold_drop_promote_merge() {
        let meta = trips();
        let mode = FlattenMode::Nested;
        let drop = canonicalize(
            mode,
            &meta,
            &SchemaAlterOp::Drop {
                column: "region".into(),
            },
            &[],
        )
        .unwrap();
        let promote = canonicalize(
            mode,
            &meta,
            &SchemaAlterOp::Promote {
                column: "n".into(),
                to: SkipprDataType::Long,
            },
            &[],
        )
        .unwrap();
        let merge = canonicalize(
            mode,
            &meta,
            &SchemaAlterOp::Merge {
                src: "price_string".into(),
                dst: "price".into(),
            },
            &[],
        )
        .unwrap();
        let folded = [drop, promote, merge]
            .iter()
            .try_fold(meta, |meta, alter| apply(meta, alter))
            .unwrap();
        assert!(folded.fields.get("region").is_none());
        assert!(folded.fields.get("price_string").is_none());
        assert_eq!(folded.fields["n"].determined_type, SkipprDataType::Long);
        assert_eq!(folded.fields["price"].field_id, 7);
        assert!(folded.fields["price"].evolution.is_empty());
    }

    #[test]
    fn fold_rejects_id_drift() {
        let meta = trips();
        let mut column = col(FlattenMode::Nested, &meta, "region");
        column.id = 99;
        assert_eq!(
            apply(meta, &CanonicalAlter::Drop { column }),
            Err(SchemaAlterError::IdNameMismatch {
                id: 99,
                name: "region".into()
            })
        );
    }

    #[test]
    fn dest_accepts_uses_source_type_and_dialect() {
        let meta = trips();
        let day = col(FlattenMode::Nested, &meta, "event_day");
        let date_to_ts = CanonicalAlter::Promote {
            column: day,
            to: SkipprDataType::Timestamp,
        };
        let milli = CanonicalAlter::Promote {
            column: AlterColumn {
                ty: SkipprDataType::TimestampMilli,
                ..col(FlattenMode::Nested, &meta, "event_day")
            },
            to: SkipprDataType::Timestamp,
        };
        let snowflake = AlterSupport::Warehouse(WarehouseDialect::Snowflake);
        assert_eq!(
            dest_accepts(snowflake, &date_to_ts),
            Err(SchemaAlterError::DestRejects(
                DestRejection::DateToTimestamp(WarehouseDialect::Snowflake)
            ))
        );
        dest_accepts(snowflake, &milli).unwrap();
        dest_accepts(AlterSupport::Iceberg, &date_to_ts).unwrap();
        assert_eq!(
            dest_accepts(AlterSupport::Warehouse(WarehouseDialect::Redshift), &milli),
            Err(SchemaAlterError::DestRejects(
                DestRejection::RedshiftTypeChange
            ))
        );
        let nested = CanonicalAlter::Drop {
            column: col(FlattenMode::Nested, &meta, "detail.zone"),
        };
        assert!(matches!(
            dest_accepts(AlterSupport::Warehouse(WarehouseDialect::Postgres), &nested),
            Err(SchemaAlterError::DestRejects(DestRejection::NestedColumn(
                _
            )))
        ));
        assert_eq!(
            dest_accepts(AlterSupport::Unsupported, &nested),
            Err(SchemaAlterError::DestRejects(DestRejection::NotTable))
        );
    }

    #[test]
    fn apply_alter_binds_host_arrow_field_ids() {
        let meta = trips();
        let schema = host_arrow(&meta, FlattenMode::Nested);
        assert!(
            schema.fields().iter().all(|f| field_id_of(f).is_some()),
            "convert_skippr_to_arrow stamps PARQUET:field_id from Metadata"
        );
        let alter = canonicalize(
            FlattenMode::Nested,
            &meta,
            &SchemaAlterOp::Rename {
                from: "region".into(),
                to: "region_code".into(),
            },
            &[],
        )
        .unwrap();
        let out = apply_alter(RecordBatch::new_empty(schema), &alter).unwrap();
        assert!(out.schema().index_of("region_code").is_ok());
        assert!(out.schema().index_of("region").is_err());
        assert_eq!(
            field_id_of(out.schema().field_with_name("region_code").unwrap()),
            Some(21)
        );
    }

    #[test]
    fn apply_alter_rename_keeps_field_id_and_checks_it() {
        let batch = one_col(
            id_field("price", DataType::Int64, 7),
            Arc::new(Int64Array::from(vec![1, 2])),
        );
        let column = AlterColumn {
            name: OutputName::parse_sql(FlattenMode::Nested, "price").unwrap(),
            id: 7,
            ty: SkipprDataType::Long,
        };
        let rename = CanonicalAlter::Rename {
            column: column.clone(),
            to_leaf: "amount".into(),
        };
        let out = apply_alter(batch.clone(), &rename).unwrap();
        assert_eq!(out.schema().field(0).name(), "amount");
        assert_eq!(field_id_of(out.schema().field(0)), Some(7));
        let wrong = CanonicalAlter::Rename {
            column: AlterColumn { id: 99, ..column },
            to_leaf: "amount".into(),
        };
        assert_eq!(
            apply_alter(batch, &wrong),
            Err(SchemaAlterError::IdNameMismatch {
                id: 99,
                name: "price".into()
            })
        );
    }

    #[test]
    fn apply_alter_is_idempotent_on_resume() {
        let meta = trips();
        let schema = host_arrow(&meta, FlattenMode::Nested);
        for op in [
            SchemaAlterOp::Rename {
                from: "region".into(),
                to: "region_code".into(),
            },
            SchemaAlterOp::Drop {
                column: "detail.zone".into(),
            },
            SchemaAlterOp::Merge {
                src: "price_string".into(),
                dst: "price".into(),
            },
            SchemaAlterOp::Promote {
                column: "n".into(),
                to: SkipprDataType::Long,
            },
        ] {
            let alter = canonicalize(FlattenMode::Nested, &meta, &op, &[]).unwrap();
            let once = apply_alter(RecordBatch::new_empty(schema.clone()), &alter).unwrap();
            let twice = apply_alter(once.clone(), &alter).unwrap();
            assert_eq!(once.schema(), twice.schema(), "{op:?}");
        }
    }

    #[test]
    fn apply_alter_merge_coalesces_and_drops_src() {
        let schema = Schema::new(vec![
            id_field("price", DataType::Utf8, 7),
            id_field("price_string", DataType::Utf8, 9),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(StringArray::from(vec![Some("keep"), None])) as ArrayRef,
                Arc::new(StringArray::from(vec![Some("ignore"), Some("fill")])) as ArrayRef,
            ],
        )
        .unwrap();
        let meta = trips();
        let merge = canonicalize(
            FlattenMode::Nested,
            &meta,
            &SchemaAlterOp::Merge {
                src: "price_string".into(),
                dst: "price".into(),
            },
            &[],
        )
        .unwrap();
        let out = apply_alter(batch, &merge).unwrap();
        assert_eq!(out.num_columns(), 1);
        let col = out
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!((col.value(0), col.value(1)), ("keep", "fill"));
    }

    #[test]
    fn apply_alter_merge_into_absent_dst_renames_and_casts_in_place() {
        let meta = ns(vec![
            field("n", SkipprDataType::Integer, 3),
            field("total", SkipprDataType::Long, 8),
        ]);
        let merge = canonicalize(
            FlattenMode::Nested,
            &meta,
            &SchemaAlterOp::Merge {
                src: "n".into(),
                dst: "total".into(),
            },
            &[],
        )
        .unwrap();
        let batch = one_col(
            Field::new("n", DataType::Int32, true),
            Arc::new(arrow::array::Int32Array::from(vec![5])),
        );
        let out = apply_alter(batch, &merge).unwrap();
        assert_eq!(out.schema().field(0).name(), "total");
        assert_eq!(out.schema().field(0).data_type(), &DataType::Int64);
    }

    #[test]
    fn apply_alter_drops_nested_leaf() {
        let region = id_field("region", DataType::Utf8, 21);
        let inner = StructArray::from(vec![(
            Arc::new(region.clone()),
            Arc::new(StringArray::from(vec!["uk"])) as ArrayRef,
        )]);
        let batch = one_col(
            Field::new("detail", DataType::Struct(vec![region].into()), true),
            Arc::new(inner),
        );
        let drop = CanonicalAlter::Drop {
            column: AlterColumn {
                name: OutputName::parse_sql(FlattenMode::Nested, "detail.region").unwrap(),
                id: 21,
                ty: SkipprDataType::String,
            },
        };
        let out = apply_alter(batch, &drop).unwrap();
        let DataType::Struct(children) = out.schema().field(0).data_type().clone() else {
            panic!("expected struct");
        };
        assert!(children.is_empty());
    }

    #[test]
    fn apply_alter_promote_date_to_timestamp_keeps_field_id() {
        let batch = one_col(
            id_field("event_day", DataType::Date32, 11),
            Arc::new(Date32Array::from(vec![Some(1)])),
        );
        let promote = CanonicalAlter::Promote {
            column: AlterColumn {
                name: OutputName::parse_sql(FlattenMode::Nested, "event_day").unwrap(),
                id: 11,
                ty: SkipprDataType::Date,
            },
            to: SkipprDataType::Timestamp,
        };
        assert!(promote.rewrites_data());
        let out = apply_alter(batch, &promote).unwrap();
        assert!(matches!(
            out.schema().field(0).data_type(),
            DataType::Timestamp(_, _)
        ));
        assert_eq!(field_id_of(out.schema().field(0)), Some(11));
    }

    fn committed(seq: u64, alter: CanonicalAlter) -> CommittedAlter {
        CommittedAlter {
            origin: AlterOrigin {
                migration: MigrationId::new(format!("m{seq}")),
                direction: MigrationDirection::Up,
            },
            seq,
            alter,
        }
    }

    /// Fold metadata and canonicalize each SQL op against the running output.
    fn log_of(
        mode: FlattenMode,
        source: &Metadata,
        ops: &[SchemaAlterOp],
    ) -> (Metadata, Vec<CommittedAlter>) {
        let mut output = source.clone();
        let mut log = Vec::new();
        for op in ops {
            let alter = canonicalize(mode, &output, op, &[]).unwrap();
            output = apply(output, &alter).unwrap();
            log.push(committed(log.len() as u64, alter));
        }
        (output, log)
    }

    #[test]
    fn apply_log_binds_each_alter_after_the_previous() {
        let source = ns(vec![
            field("a", SkipprDataType::String, 1),
            field("b", SkipprDataType::String, 2),
            field("c", SkipprDataType::String, 3),
        ]);
        let (output, log) = log_of(
            FlattenMode::Nested,
            &source,
            &[
                SchemaAlterOp::Drop { column: "a".into() },
                SchemaAlterOp::Rename {
                    from: "c".into(),
                    to: "a".into(),
                },
                SchemaAlterOp::Drop { column: "a".into() },
            ],
        );
        let out = apply_log(
            RecordBatch::new_empty(host_arrow(&source, FlattenMode::Nested)),
            &log,
        )
        .unwrap();
        assert_eq!(
            sorted_names(&out.schema()),
            sorted_names(&host_arrow(&output, FlattenMode::Nested))
        );
        assert_eq!(sorted_names(&out.schema()), vec!["b".to_string()]);
    }

    fn candidate_ops(mode: FlattenMode, output: &Metadata) -> Vec<SchemaAlterOp> {
        let mut leaves = Vec::new();
        fn walk(
            node: &Metadata,
            mode: FlattenMode,
            prefix: &mut Vec<String>,
            out: &mut Vec<String>,
        ) {
            for (key, child) in node.fields.iter() {
                prefix.push(segment_of(key, child).to_string());
                if child.fields.is_empty() {
                    let refs: Vec<&str> = prefix.iter().map(String::as_str).collect();
                    out.push(OutputName::from_segments(mode, &refs).column().to_string());
                } else {
                    walk(child, mode, prefix, out);
                }
                prefix.pop();
            }
        }
        walk(output, mode, &mut Vec::new(), &mut leaves);
        leaves.sort();
        let mut ops = Vec::new();
        for (i, leaf) in leaves.iter().enumerate() {
            ops.push(SchemaAlterOp::Drop {
                column: leaf.clone(),
            });
            ops.push(SchemaAlterOp::Rename {
                from: leaf.clone(),
                to: format!("r{i}_{}", leaf.len()),
            });
            ops.push(SchemaAlterOp::Promote {
                column: leaf.clone(),
                to: SkipprDataType::Long,
            });
            for other in &leaves {
                ops.push(SchemaAlterOp::Merge {
                    src: leaf.clone(),
                    dst: other.clone(),
                });
            }
        }
        ops
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// arrow(fold(source, log)) == apply_log(arrow(source)) in both modes.
        #[test]
        fn fold_equivalence(flat in any::<bool>(), picks in proptest::collection::vec(any::<prop::sample::Index>(), 1..6)) {
            let mode = FlattenMode::from_flattened(flat);
            let source = trips();
            let mut output = source.clone();
            let mut log = Vec::new();
            for pick in picks {
                let ops = candidate_ops(mode, &output);
                if ops.is_empty() {
                    break;
                }
                let Ok(alter) = canonicalize(mode, &output, pick.get(&ops), &[]) else {
                    continue;
                };
                output = apply(output, &alter).unwrap();
                log.push(committed(log.len() as u64, alter));
            }
            let rewritten = apply_log(RecordBatch::new_empty(host_arrow(&source, mode)), &log).unwrap();
            prop_assert_eq!(sorted_names(&rewritten.schema()), sorted_names(&host_arrow(&output, mode)));
        }
    }

    #[test]
    fn ingest_keys_map_renamed_top_level_keys_only() {
        let source = trips();
        let (flat_output, _) = log_of(
            FlattenMode::Flat,
            &source,
            &[SchemaAlterOp::Rename {
                from: "detail_truck_reg".into(),
                to: "truck_registration".into(),
            }],
        );
        assert!(IngestKeys::new(&source, &flat_output).is_empty());

        let (output, _) = log_of(
            FlattenMode::Nested,
            &source,
            &[SchemaAlterOp::Rename {
                from: "price".into(),
                to: "amount".into(),
            }],
        );
        let keys = IngestKeys::new(&source, &output);
        let mut rec = serde_json::json!({"amount": "1.5", "region": "uk"})
            .as_object()
            .unwrap()
            .clone();
        keys.rewrite(&mut rec).unwrap();
        assert_eq!(rec.get("price"), Some(&Value::from("1.5")));
        assert!(rec.get("amount").is_none());

        let mut both = serde_json::json!({"amount": "1", "price": "2"})
            .as_object()
            .unwrap()
            .clone();
        assert!(matches!(
            keys.rewrite(&mut both),
            Err(SchemaAlterError::IngestKeyCollision { .. })
        ));
    }

    #[test]
    fn stream_rename_is_simultaneous_and_collision_safe() {
        let schema = Schema::new(vec![
            id_field("a", DataType::Int64, 1),
            id_field("b", DataType::Int64, 2),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(Int64Array::from(vec![1])) as ArrayRef,
                Arc::new(Int64Array::from(vec![2])) as ArrayRef,
            ],
        )
        .unwrap();
        let swapped = rename_columns(
            batch.clone(),
            &[("a".into(), "b".into()), ("b".into(), "a".into())],
        )
        .unwrap();
        assert_eq!(swapped.schema().field(0).name(), "b");
        assert_eq!(field_id_of(swapped.schema().field(0)), Some(1));
        assert_eq!(
            rename_columns(batch.clone(), &[("a".into(), "b".into())]),
            Err(SchemaAlterError::ColumnExists("b".into()))
        );
        let dotted = rename_columns(batch, &[("a".into(), "price.usd".into())]).unwrap();
        assert_eq!(dotted.schema().field(0).name(), "price.usd");
    }

    #[test]
    fn stream_cast_keeps_field_id() {
        let batch = one_col(
            id_field("n", DataType::Int64, 7),
            Arc::new(Int64Array::from(vec![1])),
        );
        let out = cast_column(batch, "n", &DataType::Utf8).unwrap();
        assert_eq!(out.schema().field(0).data_type(), &DataType::Utf8);
        assert_eq!(field_id_of(out.schema().field(0)), Some(7));
    }
}
