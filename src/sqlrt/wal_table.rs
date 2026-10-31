use std::collections::HashSet;
use std::sync::Arc;

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::logical_expr::Expr;
use datafusion::physical_expr::expressions::col;
use datafusion::physical_plan::projection::{ProjectionExec, ProjectionExpr};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties};
use skippr_lease::PipelineKey;

use crate::buffer::wal_reader::PinnedSlice;
pub use crate::buffer::wal_reader::WalPinLost;
use crate::plugins::cdc::{WalPartKind, WalRowMeta};
use crate::sqlrt::cdc_reconcile::{
    attach_append_sidecar, attach_cdc_sidecar, cdc_sidecar_schema, is_cdc_sidecar_field,
};
use crate::sqlrt::read_boundary::ReadBoundary;
use crate::sqlrt::wal_slice;

/// Live WAL provider. `scan` pins the live ordinal set; `execute` must not re-select.
#[derive(Debug)]
pub struct WalTableProvider {
    pub schema: SchemaRef,
    pub pipeline: PipelineKey,
    pub namespace: String,
    pub exclude_slices: HashSet<crate::wal_slice_ref::WalSliceRef>,
    pub include_cdc_sidecar: bool,
}

impl WalTableProvider {
    pub fn new(
        schema: SchemaRef,
        pipeline: PipelineKey,
        namespace: String,
        exclude_slices: HashSet<crate::wal_slice_ref::WalSliceRef>,
    ) -> Self {
        Self {
            schema,
            pipeline,
            namespace,
            exclude_slices,
            include_cdc_sidecar: false,
        }
    }

    pub fn with_cdc_sidecar(mut self, include: bool) -> Self {
        self.include_cdc_sidecar = include;
        self
    }
}

pub(crate) fn project_batch(batch: RecordBatch, schema: &SchemaRef) -> Result<RecordBatch> {
    let mut columns = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        let idx = batch
            .schema()
            .fields()
            .iter()
            .position(|src| field_id_or_name(src, field));
        if let Some(idx) = idx {
            columns.push(batch.column(idx).clone());
        } else {
            columns.push(arrow::array::new_null_array(
                field.data_type(),
                batch.num_rows(),
            ));
        }
    }
    RecordBatch::try_new_with_options(
        schema.clone(),
        columns,
        &arrow::array::RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
    )
    .map_err(|err| DataFusionError::ArrowError(Box::new(err), None))
}

pub(crate) fn projected_schema(
    schema: &SchemaRef,
    projection: Option<&Vec<usize>>,
) -> Result<SchemaRef> {
    match projection {
        Some(indexes) => {
            Ok(Arc::new(schema.project(indexes).map_err(|err| {
                DataFusionError::ArrowError(Box::new(err), None)
            })?))
        }
        None => Ok(schema.clone()),
    }
}

/// Make a physical plan's schema match the logical projection, including
/// `COUNT(*)`'s empty projection (0 fields, rows preserved).
pub(crate) fn align_exec_to_schema(
    plan: Arc<dyn ExecutionPlan>,
    schema: SchemaRef,
) -> Result<Arc<dyn ExecutionPlan>> {
    let input = plan.schema();
    if names_and_types_match(&input, &schema) {
        return Ok(plan);
    }
    let exprs = schema
        .fields()
        .iter()
        .map(|field| {
            Ok(ProjectionExpr {
                expr: col(field.name(), &input)?,
                alias: field.name().clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Arc::new(ProjectionExec::try_new(exprs, plan)?))
}

fn names_and_types_match(left: &SchemaRef, right: &SchemaRef) -> bool {
    left.fields().len() == right.fields().len()
        && left
            .fields()
            .iter()
            .zip(right.fields())
            .all(|(a, b)| a.name() == b.name() && a.data_type() == b.data_type())
}

fn field_id_or_name(src: &arrow::datatypes::Field, target: &arrow::datatypes::Field) -> bool {
    if src.name() == target.name() {
        return true;
    }
    let id = |field: &arrow::datatypes::Field| {
        field
            .metadata()
            .get("PARQUET:field_id")
            .or_else(|| field.metadata().get("iceberg.field-id"))
            .cloned()
    };
    match (id(src), id(target)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Bounded replans after [`WalPinLost`]. Not an env knob.
pub const QUERY_PIN_RETRY_CEILING: u32 = 3;

/// True when execute lost a pinned payload. Flight SQL may only carry the Display text.
pub fn is_wal_pin_lost(err: &DataFusionError) -> bool {
    if let DataFusionError::External(inner) = err {
        if inner.downcast_ref::<WalPinLost>().is_some() {
            return true;
        }
    }
    is_wal_pin_lost_text(&err.to_string())
}

pub fn is_wal_pin_lost_text(msg: &str) -> bool {
    msg.contains("was reclaimed after the query pinned it")
}

fn payload_err(err: std::io::Error, slice: &PinnedSlice) -> DataFusionError {
    if err.kind() == std::io::ErrorKind::NotFound {
        DataFusionError::External(Box::new(WalPinLost {
            segment_id: slice.segment_id.clone(),
            start: slice.entry.start,
        }))
    } else {
        DataFusionError::Execution(err.to_string())
    }
}

fn read_pinned_slice(
    slice: &PinnedSlice,
    schema: &SchemaRef,
    include_cdc_sidecar: bool,
) -> Result<Vec<RecordBatch>> {
    let batches = wal_slice::read_slice_batches(&slice.location, &slice.entry)
        .map_err(|err| payload_err(err, slice))?;
    let part_meta = if include_cdc_sidecar {
        wal_slice::read_slice_part_meta(&slice.location, &slice.entry)
            .map_err(|err| payload_err(err, slice))?
    } else {
        None
    };
    let ordinal = slice.entry.ordinal;
    let user_schema = user_schema_without_sidecar(schema);
    let mut out = Vec::new();
    let mut row_base = 0usize;
    for batch in batches {
        let user = if user_schema.fields().is_empty() {
            batch
        } else {
            project_batch(batch, &user_schema)?
        };
        if !include_cdc_sidecar {
            out.push(if schema.fields().is_empty() {
                user
            } else {
                project_batch(user, schema)?
            });
            continue;
        }
        let rows = user.num_rows();
        let with_sidecar = attach_part_sidecar(
            user,
            part_meta.as_ref(),
            slice.seq.get(),
            ordinal,
            row_base as u64,
        )?;
        row_base += rows;
        out.push(if names_and_types_match(&with_sidecar.schema(), schema) {
            with_sidecar
        } else {
            project_batch(with_sidecar, schema)?
        });
    }
    if let Some(meta) = part_meta.as_ref() {
        if meta.kind == WalPartKind::Cdc && row_base != meta.rows.len() {
            return Err(DataFusionError::Execution(format!(
                "CDC WAL part meta row count {} does not match Arrow rows {row_base}",
                meta.rows.len()
            )));
        }
    }
    Ok(out)
}

fn attach_part_sidecar(
    user: RecordBatch,
    part_meta: Option<&crate::plugins::cdc::WalPartMeta>,
    seq: u64,
    ordinal: u32,
    row_base: u64,
) -> Result<RecordBatch> {
    let rows = user.num_rows();
    match part_meta {
        Some(meta) if meta.kind == WalPartKind::Cdc => {
            let start = row_base as usize;
            let end = start + rows;
            if end > meta.rows.len() {
                return Err(DataFusionError::Execution(format!(
                    "CDC WAL part meta row count {} does not cover Arrow rows {end}",
                    meta.rows.len()
                )));
            }
            attach_cdc_from_meta(user, &meta.rows[start..end], seq, ordinal, row_base)
        }
        _ => attach_append_sidecar(user, seq, ordinal, row_base),
    }
}

fn user_schema_without_sidecar(schema: &SchemaRef) -> SchemaRef {
    let fields: Vec<_> = schema
        .fields()
        .iter()
        .filter(|f| !is_cdc_sidecar_field(f.name()))
        .map(|f| f.as_ref().clone())
        .collect();
    Arc::new(arrow::datatypes::Schema::new(fields))
}

fn attach_cdc_from_meta(
    batch: RecordBatch,
    rows: &[WalRowMeta],
    seq: u64,
    ordinal: u32,
    row_base: u64,
) -> Result<RecordBatch> {
    let mutations: Vec<_> = rows.iter().map(|r| r.mutation).collect();
    let tokens: Vec<_> = rows.iter().map(|r| r.order_token.clone()).collect();
    attach_cdc_sidecar(batch, &mutations, &tokens, seq, ordinal, row_base)
}

#[async_trait::async_trait]
impl TableProvider for WalTableProvider {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let user = projected_schema(&self.schema, projection)?;
        let schema = if self.include_cdc_sidecar {
            cdc_sidecar_schema(&user)
        } else {
            user
        };
        let pinned = ReadBoundary::pin_wal(&self.pipeline, &self.namespace, &self.exclude_slices)
            .map_err(|err| DataFusionError::Plan(err.to_string()))?;
        pinned.emit_query_pin(&self.namespace);
        Ok(Arc::new(WalScanExec::new(
            schema,
            pinned.wal,
            limit,
            self.include_cdc_sidecar,
            self.namespace.clone(),
        )?))
    }
}

/// Physical WAL leaf. Slices are pinned at `scan`; execute must not re-select.
#[derive(Debug, Clone)]
pub struct WalScanExec {
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
    pinned: Vec<PinnedSlice>,
    limit: Option<usize>,
    include_cdc_sidecar: bool,
    namespace: String,
}

impl WalScanExec {
    pub fn new(
        schema: SchemaRef,
        pinned: Vec<PinnedSlice>,
        limit: Option<usize>,
        include_cdc_sidecar: bool,
        namespace: String,
    ) -> Result<Self> {
        let properties = Arc::new(PlanProperties::new(
            datafusion::physical_expr::EquivalenceProperties::new(schema.clone()),
            datafusion::physical_plan::Partitioning::UnknownPartitioning(1),
            datafusion::physical_plan::execution_plan::EmissionType::Incremental,
            datafusion::physical_plan::execution_plan::Boundedness::Bounded,
        ));
        Ok(Self {
            schema,
            properties,
            pinned,
            limit,
            include_cdc_sidecar,
            namespace,
        })
    }

    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn next_limited(remaining: &mut Option<usize>, batch: RecordBatch) -> Option<RecordBatch> {
        match remaining {
            None => Some(batch),
            Some(0) => None,
            Some(left) if batch.num_rows() <= *left => {
                *left -= batch.num_rows();
                Some(batch)
            }
            Some(left) => {
                let sliced = batch.slice(0, *left);
                *left = 0;
                Some(sliced)
            }
        }
    }
}

impl DisplayAs for WalScanExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "WalScanExec(namespace={}, slices={})",
            self.namespace,
            self.pinned.len()
        )
    }
}

impl ExecutionPlan for WalScanExec {
    fn name(&self) -> &str {
        "WalScanExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        Vec::new()
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn apply_expressions(
        &self,
        _f: &mut dyn FnMut(
            &Arc<dyn datafusion::physical_plan::PhysicalExpr>,
        ) -> Result<datafusion::common::tree_node::TreeNodeRecursion>,
    ) -> Result<datafusion::common::tree_node::TreeNodeRecursion> {
        Ok(datafusion::common::tree_node::TreeNodeRecursion::Continue)
    }

    fn execute(
        &self,
        partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        if partition != 0 {
            return Err(DataFusionError::Internal(format!(
                "WalScanExec has one partition, got {partition}"
            )));
        }
        let schema = self.schema.clone();
        let include_cdc = self.include_cdc_sidecar;
        let mut remaining = self.limit;
        let mut out = Vec::new();
        for slice in &self.pinned {
            if remaining == Some(0) {
                break;
            }
            for batch in read_pinned_slice(slice, &schema, include_cdc)? {
                match Self::next_limited(&mut remaining, batch) {
                    Some(batch) => out.push(Ok(batch)),
                    None => break,
                }
            }
        }
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            self.schema.clone(),
            futures::stream::iter(out),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlrt::stream_plan::StreamingBatchesExec;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use skippr_lease::{PipelineKey, PipelinePaths, SegmentId};

    #[test]
    fn project_batch_empty_schema_keeps_row_count() {
        use datafusion::arrow::array::Int64Array;
        use datafusion::arrow::datatypes::{DataType, Field, Schema};

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vec![1, 2, 3]))]).unwrap();
        let empty = Arc::new(Schema::empty());
        let projected = project_batch(batch, &empty).unwrap();
        assert_eq!(projected.num_rows(), 3);
        assert_eq!(projected.num_columns(), 0);
    }

    #[test]
    fn wal_pin_lost_is_typed_and_retry_ceiling_is_fixed() {
        assert_eq!(QUERY_PIN_RETRY_CEILING, 3);
        let lost = DataFusionError::External(Box::new(WalPinLost {
            segment_id: SegmentId::new("gone-seg").unwrap(),
            start: 0,
        }));
        assert!(is_wal_pin_lost(&lost));
        assert!(is_wal_pin_lost_text(&lost.to_string()));
        assert!(!is_wal_pin_lost(&DataFusionError::Plan(
            "WAL catalog for pipeline p is not open".into()
        )));
        assert!(!is_wal_pin_lost_text("no ready Flight SQL node"));
    }

    #[test]
    fn provider_stores_pipeline_identity() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let key = PipelineKey::new("t", "w", "wal-identity").unwrap();
        let gone = crate::wal_slice_ref::WalSliceRef::try_from_parts("gone", 0).unwrap();
        let provider =
            WalTableProvider::new(schema, key, "ns".into(), HashSet::from([gone.clone()]));
        assert_eq!(provider.namespace, "ns");
        assert_eq!(provider.exclude_slices, HashSet::from([gone]));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn missing_segment_file_raises_wal_pin_lost() {
        use crate::buffer::wal_handle::remove_wal_handle;
        use crate::buffer::wal_reader::{
            clear_query_readers, install_query_reader, reader_with_live_slice,
        };

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let key = PipelineKey::new("t", "w", "wal-missing").unwrap();
        remove_wal_handle(&key);
        clear_query_readers();
        install_query_reader(reader_with_live_slice(key.clone(), "ns", "gone-seg")).unwrap();
        let provider =
            WalTableProvider::new(schema.clone(), key.clone(), "ns".into(), HashSet::new());
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        assert_eq!(plan.schema().fields().len(), 1);
        let err = plan.execute(0, ctx.task_ctx()).err().unwrap_or_else(|| {
            DataFusionError::Internal("missing payload must fail at execute".into())
        });
        let text = err.to_string();
        assert!(
            text.contains("reclaimed after the query pinned it") || text.contains("WalPinLost"),
            "missing payload must be WalPinLost, got {text}"
        );
        clear_query_readers();
        remove_wal_handle(&key);
    }

    #[tokio::test]
    async fn empty_projection_keeps_rows_and_drops_columns() {
        use datafusion::arrow::array::Int64Array;
        use datafusion::arrow::datatypes::{DataType, Field, Schema};
        use datafusion::physical_plan::common::collect;

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("n", DataType::Int64, true),
        ]));
        let batch = datafusion::arrow::array::RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3])),
                Arc::new(Int64Array::from(vec![4, 5, 6])),
            ],
        )
        .unwrap();
        let input = Arc::new(StreamingBatchesExec::from_batches(schema, vec![batch]));
        let plan = align_exec_to_schema(input, Arc::new(Schema::empty())).unwrap();
        assert_eq!(plan.schema().fields().len(), 0);
        let ctx = datafusion::prelude::SessionContext::new();
        let batches = collect(plan.execute(0, ctx.task_ctx()).unwrap())
            .await
            .unwrap();
        assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 3);
        assert!(batches.iter().all(|b| b.num_columns() == 0));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn empty_wal_scan_honors_projection() {
        use crate::buffer::wal_handle::remove_wal_handle;
        use crate::buffer::wal_reader::{
            clear_query_readers, install_query_reader, reader_with_live_slice,
        };

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("ts", DataType::Int64, true),
        ]));
        let key = PipelineKey::new("t", "w", "wal-empty-proj").unwrap();
        remove_wal_handle(&key);
        clear_query_readers();
        install_query_reader(reader_with_live_slice(key.clone(), "other", "seg")).unwrap();
        let provider = WalTableProvider::new(schema, key.clone(), "ns".into(), HashSet::new());
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider
            .scan(&ctx.state(), Some(&vec![0]), &[], None)
            .await
            .unwrap();
        assert_eq!(plan.schema().fields().len(), 1);
        assert_eq!(plan.schema().field(0).name(), "id");
        clear_query_readers();
        remove_wal_handle(&key);
    }

    #[tokio::test]
    async fn wal_scan_pins_ordinals_at_plan_and_ignores_later_segments() {
        use crate::buffer::segment_file::{PartitionKey, WalPayload};
        use crate::buffer::wal_handle::{
            install_wal_handle, memory_catalog_for_test, remove_wal_handle, PipelineWalHandle,
        };
        use crate::helpers::offset_store::{OffsetStore, SledOffsetStore};
        use arrow::array::StringArray;
        use std::collections::HashMap;
        use std::sync::Arc as StdArc;

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let key = PipelineKey::new("t", "w", "wal-pin-plan").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = PipelinePaths::legacy_disk(dir.path());
        let fence = skippr_wal_catalog::CatalogFence::local();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.path().join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: StdArc<dyn OffsetStore> = StdArc::new(SledOffsetStore::new(tree));
        let handle = PipelineWalHandle::new(key.clone(), paths.clone(), catalog, offsets, fence);
        install_wal_handle(handle.clone());
        let part = PartitionKey {
            sink_ref: "data_sinks.iceberg".into(),
            namespace: "ns".into(),
            partition: "p0".into(),
            time: None,
            schema_fingerprint: "fp".into(),
        };
        let mut first = HashMap::new();
        first.insert(
            part.clone(),
            WalPayload::Arrow(vec![RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(StringArray::from(vec!["evt-a"]))],
            )
            .unwrap()]),
        );
        handle
            .persist_segment(
                "pinned-seg",
                &HashMap::new(),
                &first,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                std::time::SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        let provider =
            WalTableProvider::new(schema.clone(), key.clone(), "ns".into(), HashSet::new());
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        let mut second = HashMap::new();
        second.insert(
            PartitionKey {
                partition: "p1".into(),
                ..part
            },
            WalPayload::Arrow(vec![RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(StringArray::from(vec!["evt-b"]))],
            )
            .unwrap()]),
        );
        handle
            .persist_segment(
                "after-pin",
                &HashMap::new(),
                &second,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                std::time::SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        let batches =
            datafusion::physical_plan::common::collect(plan.execute(0, ctx.task_ctx()).unwrap())
                .await
                .unwrap();
        remove_wal_handle(&key);
        let ids: Vec<String> = batches
            .iter()
            .flat_map(|b| {
                b.column(0)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap()
                    .iter()
                    .map(|v| v.unwrap().to_string())
            })
            .collect();
        assert_eq!(ids, vec!["evt-a".to_string()]);
        assert!(!ids.contains(&"evt-b".to_string()));
    }

    #[tokio::test]
    async fn reclaim_between_scan_and_execute_is_wal_pin_lost() {
        use crate::buffer::segment_file::{PartitionKey, WalPayload};
        use crate::buffer::wal_handle::{
            install_wal_handle, memory_catalog_for_test, remove_wal_handle, PipelineWalHandle,
        };
        use crate::helpers::offset_store::{OffsetStore, SledOffsetStore};
        use arrow::array::StringArray;
        use std::collections::HashMap;
        use std::sync::Arc as StdArc;

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let key = PipelineKey::new("t", "w", "wal-reclaim").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = PipelinePaths::legacy_disk(dir.path());
        let fence = skippr_wal_catalog::CatalogFence::local();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.path().join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: StdArc<dyn OffsetStore> = StdArc::new(SledOffsetStore::new(tree));
        let handle = PipelineWalHandle::new(key.clone(), paths.clone(), catalog, offsets, fence);
        install_wal_handle(handle.clone());
        let mut batches = HashMap::new();
        let part = PartitionKey {
            sink_ref: "data_sinks.iceberg".into(),
            namespace: "ns".into(),
            partition: "p0".into(),
            time: None,
            schema_fingerprint: "fp".into(),
        };
        batches.insert(
            part,
            WalPayload::Arrow(vec![RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(StringArray::from(vec!["evt-a"]))],
            )
            .unwrap()]),
        );
        handle
            .persist_segment(
                "live-seg",
                &HashMap::new(),
                &batches,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                std::time::SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        let provider = WalTableProvider::new(schema, key.clone(), "ns".into(), HashSet::new());
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        handle
            .complete_slices_and_reclaim("live-seg", &[0])
            .await
            .unwrap();
        let err = match plan.execute(0, ctx.task_ctx()) {
            Err(err) => err,
            Ok(stream) => datafusion::physical_plan::common::collect(stream)
                .await
                .err()
                .expect("reclaimed payload must fail"),
        };
        remove_wal_handle(&key);
        let text = err.to_string();
        assert!(
            text.contains("reclaimed after the query pinned it") || text.contains("WalPinLost"),
            "reclaimed payload must be WalPinLost, got {text}"
        );
    }

    #[test]
    fn s3_payloads_are_decoded_from_catalog_bytes() {
        let src = include_str!("wal_table.rs");
        assert!(
            !src.contains(concat!(
                "PayloadLocation::S3 { .. } => return Ok",
                "(Vec::new())"
            )),
            "S3 catalog rows must be scanned, not treated as empty"
        );
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            prod.contains("wal_slice::read_slice_batches"),
            "WalScanExec S3/disk decode is sqlrt::wal_slice, not payload_bytes SHA"
        );
        assert!(!prod.contains("payload_bytes"));
    }

    #[test]
    fn query_wal_scan_does_not_admit_or_read_foot() {
        let src = include_str!("wal_table.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !prod.contains("admit_payload"),
            "WalScanExec must not hash/FOOT-admit payload"
        );
        assert!(
            !prod.contains("read_index_path"),
            "WalScanExec must use catalog SliceDirEntry on wal#idx#, not FOOT"
        );
        assert!(!prod.contains("CatalogKey::Index"));
        assert!(!prod.contains("wal_handle_for"));
        assert!(
            prod.contains("wal_slice::read_slice_batches"),
            "query and STREAM share sqlrt::wal_slice"
        );
        assert!(!prod.contains("fn read_batches"));
        assert!(!prod.contains("fn read_ordinal_batches"));
        assert!(!prod.contains("payload_bytes"));
        assert!(prod.contains("ReadBoundary::pin_wal"));
        assert!(prod.contains("read_pinned_slice"));
        assert!(!prod.contains("select_live_ordinals"));
        assert!(prod.contains("wal_slice::read_slice_part_meta"));
    }

    async fn persist_cdc_segment(
        handle: &crate::buffer::wal_handle::PipelineWalHandle,
        segment_id: &str,
        id: &str,
        name: &str,
        mutation: crate::plugins::cdc::MutationKind,
        order: u8,
    ) {
        use crate::buffer::segment_file::{PartitionKey, WalPayload};
        use crate::plugins::cdc::{WalPartMeta, WalRowMeta};
        use arrow::array::StringArray;

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("name", DataType::Utf8, true),
        ]));
        let part = PartitionKey {
            sink_ref: "data_sinks.iceberg".into(),
            namespace: "ns".into(),
            partition: "p0".into(),
            time: None,
            schema_fingerprint: "fp".into(),
        };
        let mut batches = std::collections::HashMap::new();
        batches.insert(
            part.clone(),
            WalPayload::Arrow(vec![RecordBatch::try_new(
                schema,
                vec![
                    Arc::new(StringArray::from(vec![id])),
                    Arc::new(StringArray::from(vec![name])),
                ],
            )
            .unwrap()]),
        );
        let meta = WalPartMeta::cdc(
            vec![WalRowMeta {
                mutation,
                event_id: id.as_bytes().to_vec(),
                order_token: vec![order],
            }],
            1,
        )
        .unwrap();
        let mut blobs = std::collections::HashMap::new();
        blobs.insert(part, bincode::serialize(&meta).unwrap());
        handle
            .persist_segment(
                segment_id,
                &std::collections::HashMap::new(),
                &batches,
                &std::collections::HashMap::new(),
                &blobs,
                &std::collections::HashMap::new(),
                std::time::SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
    }

    fn exclude_segment(
        handle: &crate::buffer::wal_handle::PipelineWalHandle,
        segment_id: &str,
    ) -> HashSet<crate::wal_slice_ref::WalSliceRef> {
        handle
            .reader()
            .live_slices("ns")
            .unwrap()
            .into_iter()
            .filter(|slice| slice.segment_id.as_str() == segment_id)
            .map(|slice| slice.slice_ref())
            .collect()
    }

    fn leased_wal(
        dir: &std::path::Path,
        key: &PipelineKey,
    ) -> Arc<crate::buffer::wal_handle::PipelineWalHandle> {
        use crate::buffer::wal_handle::{memory_catalog_for_test, PipelineWalHandle};
        use crate::helpers::offset_store::{OffsetStore, SledOffsetStore};
        use std::sync::Arc as StdArc;

        let paths = PipelinePaths::legacy_disk(dir);
        let fence = skippr_wal_catalog::CatalogFence::local();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: StdArc<dyn OffsetStore> = StdArc::new(SledOffsetStore::new(tree));
        PipelineWalHandle::new(key.clone(), paths, catalog, offsets, fence)
    }

    #[tokio::test]
    async fn cdc_sidecar_loads_from_disk_part_meta() {
        use crate::buffer::wal_handle::{install_wal_handle, remove_wal_handle};
        use crate::plugins::cdc::MutationKind;
        use crate::sqlrt::cdc_reconcile::{CDC_MUTATION_COL, CDC_ORDER_COL, CDC_SEQ_COL};
        use arrow::array::{BinaryArray, UInt8Array};

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("name", DataType::Utf8, true),
        ]));
        let key = PipelineKey::new("t", "w", "wal-cdc-sidecar").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_wal(dir.path(), &key);
        install_wal_handle(handle.clone());
        persist_cdc_segment(
            &handle,
            "cdc-seg",
            "1",
            "alpha-prime",
            MutationKind::Update,
            2,
        )
        .await;
        let provider = WalTableProvider::new(schema, key.clone(), "ns".into(), HashSet::new())
            .with_cdc_sidecar(true);
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        let batches =
            datafusion::physical_plan::common::collect(plan.execute(0, ctx.task_ctx()).unwrap())
                .await
                .unwrap();
        remove_wal_handle(&key);
        assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
        let batch = &batches[0];
        let mut_col = batch
            .column_by_name(CDC_MUTATION_COL)
            .unwrap()
            .as_any()
            .downcast_ref::<UInt8Array>()
            .unwrap();
        assert_eq!(
            mut_col.value(0),
            crate::sqlrt::cdc_reconcile::mutation_to_u8(MutationKind::Update)
        );
        let order = batch
            .column_by_name(CDC_ORDER_COL)
            .unwrap()
            .as_any()
            .downcast_ref::<BinaryArray>()
            .unwrap();
        assert_eq!(order.value(0), &[2]);
        let seq = batch
            .column_by_name(CDC_SEQ_COL)
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::UInt64Array>()
            .unwrap();
        assert_eq!(seq.value(0), handle.catalog_seq_seen().unwrap());
        let name = batch
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        assert_eq!(name.value(0), "alpha-prime");
    }

    #[tokio::test]
    async fn mixed_append_missing_payload_is_wal_pin_lost() {
        use crate::buffer::segment_file::{PartitionKey, WalPayload};
        use crate::buffer::wal_handle::{install_wal_handle, remove_wal_handle};
        use crate::plugins::cdc::MutationKind;
        use arrow::array::StringArray;

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("name", DataType::Utf8, true),
        ]));
        let key = PipelineKey::new("t", "w", "wal-cdc-mixed").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_wal(dir.path(), &key);
        install_wal_handle(handle.clone());
        persist_cdc_segment(&handle, "cdc-live", "1", "prime", MutationKind::Update, 1).await;
        persist_cdc_segment(&handle, "cdc-gone", "9", "missing", MutationKind::Insert, 1).await;
        let mut append = std::collections::HashMap::new();
        append.insert(
            PartitionKey {
                sink_ref: "data_sinks.iceberg".into(),
                namespace: "ns".into(),
                partition: "p-append".into(),
                time: None,
                schema_fingerprint: "fp".into(),
            },
            WalPayload::Arrow(vec![RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(StringArray::from(vec!["2"])),
                    Arc::new(StringArray::from(vec!["extra"])),
                ],
            )
            .unwrap()]),
        );
        handle
            .persist_segment(
                "append-seg",
                &std::collections::HashMap::new(),
                &append,
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                std::time::SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        persist_cdc_segment(
            &handle,
            "cdc-excluded",
            "3",
            "hidden",
            MutationKind::Insert,
            1,
        )
        .await;
        let gone = handle.paths().segs.join("cdc-gone.seg");
        let _ = std::fs::remove_file(&gone);
        let provider = WalTableProvider::new(
            schema,
            key.clone(),
            "ns".into(),
            exclude_segment(&handle, "cdc-excluded"),
        )
        .with_cdc_sidecar(true);
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        let err = plan.execute(0, ctx.task_ctx()).err().unwrap_or_else(|| {
            DataFusionError::Internal("missing payload must fail at execute".into())
        });
        remove_wal_handle(&key);
        assert!(
            is_wal_pin_lost(&err),
            "unreadable pinned slice must be WalPinLost, got {err}"
        );
    }

    #[tokio::test]
    async fn disk_cdc_merge_replaces_lake_row() {
        use crate::buffer::wal_handle::{install_wal_handle, remove_wal_handle};
        use crate::plugins::cdc::MutationKind;
        use crate::sqlrt::cdc_reconcile::CdcReconcileExec;
        use arrow::array::StringArray;
        use datafusion::datasource::MemTable;
        use datafusion::physical_plan::common::collect;

        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("name", DataType::Utf8, true),
        ]));
        let lake = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["1", "2"])),
                Arc::new(StringArray::from(vec!["alpha", "beta"])),
            ],
        )
        .unwrap();
        let parquet_dir = tempfile::tempdir().unwrap();
        let parquet_path = parquet_dir.path().join("lake.parquet");
        {
            let file = std::fs::File::create(&parquet_path).unwrap();
            let mut writer =
                parquet::arrow::ArrowWriter::try_new(file, schema.clone(), None).unwrap();
            writer.write(&lake).unwrap();
            writer.close().unwrap();
        }
        assert!(parquet_path.is_file());
        let key = PipelineKey::new("t", "w", "wal-cdc-merge").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_wal(dir.path(), &key);
        install_wal_handle(handle.clone());
        persist_cdc_segment(
            &handle,
            "wal-cdc",
            "1",
            "alpha-prime",
            MutationKind::Update,
            3,
        )
        .await;
        let wal = Arc::new(
            WalTableProvider::new(schema.clone(), key.clone(), "ns".into(), HashSet::new())
                .with_cdc_sidecar(true),
        );
        let iceberg = Arc::new(MemTable::try_new(schema.clone(), vec![vec![lake]]).unwrap());
        let ctx = datafusion::prelude::SessionContext::new();
        let iceberg_plan = iceberg.scan(&ctx.state(), None, &[], None).await.unwrap();
        let wal_plan = wal.scan(&ctx.state(), None, &[], None).await.unwrap();
        let merge = Arc::new(
            CdcReconcileExec::try_new(iceberg_plan, wal_plan, vec!["id".into()], schema.clone())
                .unwrap(),
        );
        let batches = collect(merge.execute(0, ctx.task_ctx()).unwrap())
            .await
            .unwrap();
        remove_wal_handle(&key);
        let mut names: Vec<(String, String)> = Vec::new();
        for batch in &batches {
            let ids = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            let name = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            for i in 0..batch.num_rows() {
                names.push((ids.value(i).to_string(), name.value(i).to_string()));
            }
        }
        assert_eq!(names.len(), 2);
        assert!(names.contains(&("1".into(), "alpha-prime".into())));
        assert!(names.contains(&("2".into(), "beta".into())));
        assert!(!names.iter().any(|(_, n)| n == "alpha"));
    }

    #[tokio::test]
    async fn union_keeps_unexcluded_slice_of_same_segment() {
        use crate::buffer::segment_file::{PartitionKey, WalPayload};
        use crate::buffer::wal_handle::{install_wal_handle, remove_wal_handle};
        use arrow::array::StringArray;
        use datafusion::datasource::MemTable;
        use datafusion::physical_plan::common::collect;

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let lake = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(StringArray::from(vec!["lake"]))],
        )
        .unwrap();
        let key = PipelineKey::new("t", "w", "wal-union-slice").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_wal(dir.path(), &key);
        install_wal_handle(handle.clone());
        let mut batches = std::collections::HashMap::new();
        for (partition, id) in [("p0", "wal-p0"), ("p1", "wal-p1")] {
            batches.insert(
                PartitionKey {
                    sink_ref: "data_sinks.iceberg".into(),
                    namespace: "ns".into(),
                    partition: partition.into(),
                    time: None,
                    schema_fingerprint: "fp".into(),
                },
                WalPayload::Arrow(vec![RecordBatch::try_new(
                    schema.clone(),
                    vec![Arc::new(StringArray::from(vec![id]))],
                )
                .unwrap()]),
            );
        }
        handle
            .persist_segment(
                "seg-part",
                &std::collections::HashMap::new(),
                &batches,
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                &std::collections::HashMap::new(),
                std::time::SystemTime::UNIX_EPOCH,
            )
            .await
            .unwrap();
        let slices = handle.reader().live_slices("ns").unwrap();
        let p0 = slices
            .iter()
            .find(|slice| slice.entry.partition == "p0")
            .unwrap();
        let exclude = HashSet::from([p0.slice_ref()]);
        let wal = Arc::new(WalTableProvider::new(
            schema.clone(),
            key.clone(),
            "ns".into(),
            exclude,
        ));
        let iceberg = Arc::new(MemTable::try_new(schema.clone(), vec![vec![lake]]).unwrap());
        let ctx = datafusion::prelude::SessionContext::new();
        let iceberg_plan = iceberg.scan(&ctx.state(), None, &[], None).await.unwrap();
        let wal_plan = wal.scan(&ctx.state(), None, &[], None).await.unwrap();
        let mut batches = collect(iceberg_plan.execute(0, ctx.task_ctx()).unwrap())
            .await
            .unwrap();
        batches.extend(
            collect(wal_plan.execute(0, ctx.task_ctx()).unwrap())
                .await
                .unwrap(),
        );
        remove_wal_handle(&key);
        let ids: Vec<String> = batches
            .iter()
            .flat_map(|b| {
                b.column(0)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap()
                    .iter()
                    .map(|v| v.unwrap().to_string())
            })
            .collect();
        assert!(ids.contains(&"lake".to_string()));
        assert!(ids.contains(&"wal-p1".to_string()));
        assert!(!ids.contains(&"wal-p0".to_string()));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn cdc_part_meta_shorter_than_arrow_is_error() {
        use crate::plugins::cdc::{MutationKind, WalPartMeta, WalRowMeta};
        use arrow::array::StringArray;

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(StringArray::from(vec!["a", "b"]))])
            .unwrap();
        let meta = WalPartMeta::cdc(
            vec![WalRowMeta {
                mutation: MutationKind::Insert,
                event_id: b"a".to_vec(),
                order_token: vec![1],
            }],
            1,
        )
        .unwrap();
        let err = attach_part_sidecar(batch, Some(&meta), 1, 0, 0).unwrap_err();
        assert!(err.to_string().contains("row count"), "{err}");
    }
}
