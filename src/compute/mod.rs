//! Bound compute IR. Declared compute is a [`ComputeKind`] in the Skippr Store journal.
//! YAML and Python `transform.compute` serialize that IR.

use std::time::Duration;

use arrow::record_batch::RecordBatch;
use serde::{Deserialize, Serialize};

use crate::plugins::cdc::WalRowMeta;

mod apply;
mod bind;
mod journal;
mod parse;
mod py;
mod stateless;
mod table;

pub use apply::{
    admit_config, apply_query_statement, bind_catalog, prefix_deletes, resolve_ddl_pipeline,
    stateless_for_namespace, ComputeApplyError,
};

pub use bind::{bind_drop_compute, bind_statement, BindCatalog, BindError};
pub use journal::{
    canonicalize_kind, commit as commit_journal, commit_log, drop_compute, ir_hash, ir_hash_kind,
    journal_key, load as load_journal, CatalogEntry, ComputeHash, ComputeLog,
};
pub use parse::{
    desugar_create, desugar_stateless, parse_watermark_delay, ComputeAlteration, UnboundCompute,
    UnboundError, UnboundMeasureKind,
};
#[cfg(test)]
pub(crate) use parse::{VIEW_ILLEGAL, VIEW_ILLEGAL_REQUIRED, VIEW_LEGAL};
#[cfg(test)]
pub(crate) use py::write_py_dir;
pub use py::{
    admit_item, apply_py_records, assert_py_matches_journal, blocks_unflattened_arrow,
    copy_wal_fingerprint, has_py, py_changed_message, py_loads, PyLoad, COPY_PY_UNADMITTED,
    PY_FILE_MAX_BYTES, PY_UNFLATTENED_ARROW,
};
pub use stateless::{apply_stateless, dest_schema, drop_invalid_event_times, StatelessSelect};
pub use table::{relation_sql, stream_join_sql, view_sql, ComputeTableProvider};

/// Non-zero Iceberg/Parquet field identity. Bind keys and measures to this, not names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct FieldId(i32);

impl FieldId {
    pub fn new(id: i32) -> Result<Self, String> {
        if id == 0 {
            return Err("FieldId must be non-zero".into());
        }
        Ok(Self(id))
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// Non-empty sequence. Empty is unrepresentable.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NonEmpty<T>(Vec<T>);

impl<T> NonEmpty<T> {
    pub fn try_from_vec(items: Vec<T>) -> Result<Self, String> {
        if items.is_empty() {
            return Err("expected at least one item".into());
        }
        Ok(Self(items))
    }

    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }
}

impl<T> TryFrom<Vec<T>> for NonEmpty<T> {
    type Error = String;
    fn try_from(items: Vec<T>) -> Result<Self, Self::Error> {
        Self::try_from_vec(items)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PipelineName(String);

impl PipelineName {
    pub fn new(name: impl Into<String>) -> Result<Self, String> {
        let name = name.into();
        if name.is_empty() || name.contains('#') || name.contains('.') || name.contains('/') {
            return Err("pipeline name must be non-empty without '.', '#', or '/'".into());
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Ident(String);

impl Ident {
    pub fn new(name: impl Into<String>) -> Result<Self, String> {
        let name = name.into();
        if name.is_empty() || name.contains('#') || name.contains('.') || name.contains('/') {
            return Err("ident must be non-empty without '.', '#', or '/'".into());
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `skipprd query` two-part name `pipeline.name`. Typed; never `rsplit`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fqn {
    pub pipeline: PipelineName,
    pub name: Ident,
}

impl Fqn {
    pub fn new(pipeline: PipelineName, name: Ident) -> Self {
        Self { pipeline, name }
    }

    /// Parser sentinel for a one-part SQL name. Not a legal operator pipeline (`_skippr` is reserved).
    pub const BARE: &'static str = "_skippr";

    pub fn from_parts(pipeline: &str, name: &str) -> Result<Self, String> {
        Ok(Self {
            pipeline: PipelineName::new(pipeline)?,
            name: Ident::new(name)?,
        })
    }

    pub fn is_bare(&self) -> bool {
        self.pipeline.as_str() == Self::BARE
    }

    /// Item `name` is `ident` or `pipeline.ident`. Bare ident uses `bound_pipeline`.
    pub fn parse_item(bound_pipeline: &str, name: &str) -> Result<Self, String> {
        match name.split_once('.') {
            Some((pipeline, ident)) if !ident.is_empty() && !ident.contains('.') => {
                Self::from_parts(pipeline, ident)
            }
            None => Self::from_parts(bound_pipeline, name),
            Some(_) => Err("compute name must be ident or pipeline.ident".into()),
        }
    }
}

impl std::fmt::Display for Fqn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.pipeline.as_str(), self.name.as_str())
    }
}

/// Event-time column. File/Iceberg partitions use this FieldId. Not a data-dir watermark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventTimeCol(FieldId);

impl EventTimeCol {
    pub fn new(id: FieldId) -> Self {
        Self(id)
    }

    pub fn field_id(self) -> FieldId {
        self.0
    }
}

/// Event-time watermark delay. Distinct from disk-space high/low percent and plugin publication watermarks.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventWatermark {
    pub on: EventTimeCol,
    pub delay: Duration,
}

impl EventWatermark {
    pub fn new(on: EventTimeCol, delay: Duration) -> Self {
        Self { on, delay }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MeasureKind {
    Sum(FieldId),
    Count,
    Min(FieldId),
    Max(FieldId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Measure {
    pub name: Ident,
    pub kind: MeasureKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Tumble {
    pub size: Duration,
    pub on: EventTimeCol,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Relation {
    PipelineTable {
        pipeline: PipelineName,
        namespace: Ident,
    },
    View {
        name: Fqn,
    },
    Latest {
        name: Fqn,
    },
    KeyedTable {
        name: Fqn,
    },
    WindowedTable {
        name: Fqn,
    },
}

/// Bound compute. Stream-stream interval joins and cross-pipeline unions are not variants.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ComputeKind {
    Stateless {
        name: Fqn,
        target: Relation,
        select: StatelessSelect,
    },
    View {
        name: Fqn,
        input: Relation,
        select: StatelessSelect,
    },
    Latest {
        name: Fqn,
        input: Relation,
        keys: NonEmpty<FieldId>,
        /// `(FieldId, descending)` from DISTINCT ON ORDER BY.
        order: Vec<(FieldId, bool)>,
    },
    KeyedTable {
        name: Fqn,
        input: Relation,
        keys: NonEmpty<FieldId>,
        measures: NonEmpty<Measure>,
    },
    WindowedTable {
        name: Fqn,
        input: Relation,
        keys: NonEmpty<FieldId>,
        window: Tumble,
        watermark: EventWatermark,
        measures: NonEmpty<Measure>,
    },
    StreamTableJoin {
        name: Fqn,
        stream: Relation,
        table: Fqn,
        on: NonEmpty<FieldId>,
    },
    /// File + handler Python map on decoded JSON objects.
    Py {
        name: Fqn,
        path: String,
        handler: String,
        digest: String,
    },
}

impl ComputeKind {
    pub fn name(&self) -> &Fqn {
        match self {
            Self::Stateless { name, .. }
            | Self::View { name, .. }
            | Self::Latest { name, .. }
            | Self::KeyedTable { name, .. }
            | Self::WindowedTable { name, .. }
            | Self::StreamTableJoin { name, .. }
            | Self::Py { name, .. } => name,
        }
    }

    pub fn catalog_key(&self) -> String {
        self.name().to_string()
    }

    pub fn is_table_backed(&self) -> bool {
        !matches!(self, Self::Stateless { .. } | Self::Py { .. })
    }

    pub fn is_stateless(&self) -> bool {
        matches!(self, Self::Stateless { .. })
    }

    pub fn store_id(&self) -> Option<&str> {
        match self {
            Self::KeyedTable { name, .. }
            | Self::WindowedTable { name, .. }
            | Self::Latest { name, .. } => Some(name.name.as_str()),
            Self::Stateless { .. }
            | Self::View { .. }
            | Self::StreamTableJoin { .. }
            | Self::Py { .. } => None,
        }
    }

    pub fn keys(&self) -> Option<&[FieldId]> {
        match self {
            Self::Latest { keys, .. }
            | Self::KeyedTable { keys, .. }
            | Self::WindowedTable { keys, .. } => Some(keys.as_slice()),
            Self::StreamTableJoin { on, .. } => Some(on.as_slice()),
            Self::Stateless { .. } | Self::View { .. } | Self::Py { .. } => None,
        }
    }

    pub fn measures(&self) -> &[Measure] {
        match self {
            Self::KeyedTable { measures, .. } | Self::WindowedTable { measures, .. } => {
                measures.as_slice()
            }
            _ => &[],
        }
    }

    pub fn is_store_table(&self) -> bool {
        matches!(
            self,
            Self::KeyedTable { .. } | Self::WindowedTable { .. } | Self::Latest { .. }
        )
    }
}

pub const PARQUET_FIELD_ID: &str = "PARQUET:field_id";

pub fn field_id_of(field: &arrow_schema::Field) -> Option<FieldId> {
    field
        .metadata()
        .get(PARQUET_FIELD_ID)
        .or_else(|| field.metadata().get("iceberg.field-id"))
        .and_then(|s| s.parse().ok())
        .and_then(|id| FieldId::new(id).ok())
}

pub fn name_for_field_id<'a>(schema: &'a arrow_schema::Schema, id: FieldId) -> Option<&'a str> {
    schema
        .fields()
        .iter()
        .find(|field| field_id_of(field) == Some(id))
        .map(|field| field.name().as_str())
}

pub fn field_with_id(
    name: &str,
    data_type: arrow_schema::DataType,
    nullable: bool,
    id: i32,
) -> arrow_schema::Field {
    let mut meta = std::collections::HashMap::new();
    meta.insert(PARQUET_FIELD_ID.to_string(), id.to_string());
    arrow_schema::Field::new(name, data_type, nullable).with_metadata(meta)
}

/// Arrow batch plus optional CDC sidecar. Lengths are equal by construction.
#[derive(Debug)]
pub struct AlignedBatch {
    pub batch: RecordBatch,
    pub cdc: Option<Vec<WalRowMeta>>,
}

impl AlignedBatch {
    pub fn new(batch: RecordBatch, cdc: Option<Vec<WalRowMeta>>) -> Result<Self, String> {
        if let Some(ref rows) = cdc {
            if rows.len() != batch.num_rows() {
                return Err(format!(
                    "AlignedBatch length mismatch: {} Arrow rows, {} CDC rows",
                    batch.num_rows(),
                    rows.len()
                ));
            }
        }
        Ok(Self { batch, cdc })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Int64Array;
    use arrow_schema::{DataType, Field, Schema};
    use std::sync::Arc;

    fn empty_batch() -> RecordBatch {
        RecordBatch::new_empty(Arc::new(Schema::new(vec![Field::new(
            "id",
            DataType::Int64,
            true,
        )])))
    }

    #[test]
    fn field_id_rejects_zero() {
        assert!(FieldId::new(0).is_err());
        assert_eq!(FieldId::new(7).unwrap().get(), 7);
    }

    #[test]
    fn nonempty_rejects_empty() {
        assert!(NonEmpty::<i32>::try_from_vec(vec![]).is_err());
        assert_eq!(NonEmpty::try_from_vec(vec![1]).unwrap().as_slice(), &[1]);
    }

    #[test]
    fn fqn_is_typed_not_rsplit() {
        let fqn = Fqn::from_parts("invoices", "customer_totals").unwrap();
        assert_eq!(fqn.to_string(), "invoices.customer_totals");
        assert!(Fqn::from_parts("invoices.extra", "t").is_err());
        assert!(PipelineName::new("a.b").is_err());
        assert_eq!(
            Fqn::parse_item("invoices", "positive_orders")
                .unwrap()
                .to_string(),
            "invoices.positive_orders"
        );
        assert_eq!(
            Fqn::parse_item("invoices", "invoices.customer_totals")
                .unwrap()
                .to_string(),
            "invoices.customer_totals"
        );
    }

    #[test]
    fn event_watermark_is_not_data_dir() {
        let prod = include_str!("mod.rs")
            .split("\nmod tests {")
            .next()
            .expect("production module");
        assert!(prod.contains("struct EventWatermark"));
        assert!(!prod.contains(&["DATA", "_DIR"].concat()));
        let wm = EventWatermark::new(
            EventTimeCol::new(FieldId::new(3).unwrap()),
            Duration::from_secs(300),
        );
        assert_eq!(wm.delay, Duration::from_secs(300));
    }

    #[test]
    fn aligned_batch_rejects_length_mismatch() {
        let batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)])),
            vec![Arc::new(Int64Array::from(vec![1, 2]))],
        )
        .unwrap();
        let err = AlignedBatch::new(
            batch,
            Some(vec![WalRowMeta::new(
                crate::plugins::cdc::MutationKind::Insert,
                b"a".to_vec(),
                vec![1],
            )]),
        )
        .unwrap_err();
        assert!(err.contains("length mismatch"), "{err}");
    }

    #[test]
    fn aligned_batch_accepts_empty_with_empty_cdc() {
        let got = AlignedBatch::new(empty_batch(), Some(Vec::new())).unwrap();
        assert_eq!(got.batch.num_rows(), 0);
        assert_eq!(got.cdc.as_ref().map(Vec::len), Some(0));
    }

    #[test]
    fn compute_kind_has_no_interval_or_union_variants() {
        let prod = include_str!("mod.rs")
            .split("\nmod tests {")
            .next()
            .expect("production module");
        assert!(!prod.contains("IntervalJoin"));
        assert!(!prod.contains("Hop"));
        assert!(!prod.contains("SESSION"));
        assert!(!prod.contains("CUMULATE"));
    }

    #[test]
    fn ident_rejects_slash() {
        assert!(Ident::new("orders/eu").is_err());
        assert!(PipelineName::new("invoices/eu").is_err());
        assert!(Fqn::from_parts("invoices", "orders/eu").is_err());
    }
}
