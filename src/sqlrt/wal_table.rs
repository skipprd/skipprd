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
use skippr_lease::{PipelineKey, SegmentId};
use skippr_wal_catalog::{CatalogKey, CatalogRow};

use crate::buffer::wal_handle;
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
    pub exclude_segment_ids: Vec<String>,
    pub include_cdc_sidecar: bool,
}

impl WalTableProvider {
    pub fn new(
        schema: SchemaRef,
        pipeline: PipelineKey,
        namespace: String,
        exclude_segment_ids: Vec<String>,
    ) -> Self {
        Self {
            schema,
            pipeline,
            namespace,
            exclude_segment_ids,
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

fn read_ordinal_batches(
    pipeline: &PipelineKey,
    segment_id: &str,
    ordinal: u32,
    namespace: &str,
    schema: &SchemaRef,
    include_cdc_sidecar: bool,
) -> Result<Vec<RecordBatch>> {
    let Some(handle) = wal_handle::wal_handle_for(pipeline) else {
        return Ok(Vec::new());
    };
    let Ok(id) = SegmentId::new(segment_id) else {
        return Ok(Vec::new());
    };
    let Ok(Some(CatalogRow::Index(index))) = handle.catalog().get(&CatalogKey::Index(id.clone()))
    else {
        return Ok(Vec::new());
    };
    let Some(entry) = index.entries.iter().find(|entry| entry.ordinal == ordinal) else {
        return Ok(Vec::new());
    };
    if entry.namespace != namespace {
        return Ok(Vec::new());
    }
    let slice = skippr_wal_catalog::SliceIndexEntry {
        segment_id: id.clone(),
        ordinal: entry.ordinal,
        namespace: entry.namespace.clone(),
        partition: entry.partition.clone(),
        start: entry.start,
        len: entry.len,
        codec: entry.codec,
        part_meta_start: entry.part_meta_start,
        part_meta_len: entry.part_meta_len,
        version: 0,
    };
    let Ok(Some(CatalogRow::Segment(seg))) = handle.catalog().get(&CatalogKey::Segment(id)) else {
        return Ok(Vec::new());
    };
    let batches = wal_slice::read_slice_batches(&seg.location, &slice)
        .map_err(|err| DataFusionError::Execution(err.to_string()))?;
    let part_meta = if include_cdc_sidecar {
        wal_slice::read_slice_part_meta(&seg.location, &slice)
            .map_err(|err| DataFusionError::Execution(err.to_string()))?
    } else {
        None
    };
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
        let with_sidecar = match part_meta.as_ref() {
            Some(meta) if meta.kind == WalPartKind::Cdc && row_base + rows <= meta.rows.len() => {
                let slice_rows = &meta.rows[row_base..row_base + rows];
                attach_cdc_from_meta(user, slice_rows, segment_id, ordinal)?
            }
            _ => attach_append_sidecar(user, segment_id, ordinal)?,
        };
        row_base += rows;
        out.push(if names_and_types_match(&with_sidecar.schema(), schema) {
            with_sidecar
        } else {
            project_batch(with_sidecar, schema)?
        });
    }
    Ok(out)
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
    segment_id: &str,
    ordinal: u32,
) -> Result<RecordBatch> {
    let mutations: Vec<_> = rows.iter().map(|r| r.mutation).collect();
    let tokens: Vec<_> = rows.iter().map(|r| r.order_token.clone()).collect();
    attach_cdc_sidecar(batch, &mutations, &tokens, segment_id, ordinal)
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
        let pinned = ReadBoundary::pin_wal(&self.pipeline, &self.exclude_segment_ids);
        Ok(Arc::new(WalScanExec::new(
            schema,
            self.pipeline.clone(),
            self.namespace.clone(),
            pinned.wal_ordinals,
            limit,
            self.include_cdc_sidecar,
        )?))
    }
}

/// Physical WAL leaf. Ordinals are pinned at `scan`; execute must not re-select.
#[derive(Debug, Clone)]
pub struct WalScanExec {
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
    pipeline: PipelineKey,
    namespace: String,
    pinned_ordinals: Vec<(String, u32)>,
    limit: Option<usize>,
    include_cdc_sidecar: bool,
}

impl WalScanExec {
    pub fn new(
        schema: SchemaRef,
        pipeline: PipelineKey,
        namespace: String,
        pinned_ordinals: Vec<(String, u32)>,
        limit: Option<usize>,
        include_cdc_sidecar: bool,
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
            pipeline,
            namespace,
            pinned_ordinals,
            limit,
            include_cdc_sidecar,
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
            "WalScanExec(pipeline={}, namespace={})",
            self.pipeline.pipeline(),
            self.namespace
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
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let ordinals = self.pinned_ordinals.clone();
        let pipeline = self.pipeline.clone();
        let namespace = self.namespace.clone();
        let schema = self.schema.clone();
        let include_cdc = self.include_cdc_sidecar;
        let mut remaining = self.limit;
        let mut out = Vec::new();
        for (segment_id, ordinal) in ordinals {
            if remaining == Some(0) {
                break;
            }
            for batch in read_ordinal_batches(
                &pipeline,
                &segment_id,
                ordinal,
                &namespace,
                &schema,
                include_cdc,
            )? {
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
    use skippr_lease::PipelineKey;

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
    fn provider_stores_pipeline_identity() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let key = PipelineKey::new("t", "w", "wal-identity").unwrap();
        let provider = WalTableProvider::new(schema, key, "ns".into(), vec!["gone".into()]);
        assert_eq!(provider.namespace, "ns");
        assert_eq!(provider.exclude_segment_ids, vec!["gone".to_string()]);
    }

    #[tokio::test]
    async fn missing_segment_file_skips_ordinal() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let key = PipelineKey::new("t", "w", "wal-missing").unwrap();
        let provider =
            WalTableProvider::new(schema.clone(), key, "ns".into(), vec!["gone-seg".into()]);
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        assert_eq!(plan.schema().fields().len(), 1);
        let batches =
            datafusion::physical_plan::common::collect(plan.execute(0, ctx.task_ctx()).unwrap())
                .await
                .unwrap();
        assert!(batches.iter().all(|batch| batch.num_rows() == 0) || batches.is_empty());
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
    async fn empty_wal_scan_honors_projection() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, true),
            Field::new("ts", DataType::Int64, true),
        ]));
        let key = PipelineKey::new("t", "w", "wal-empty-proj").unwrap();
        let provider = WalTableProvider::new(schema, key, "ns".into(), vec![]);
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider
            .scan(&ctx.state(), Some(&vec![0]), &[], None)
            .await
            .unwrap();
        assert_eq!(plan.schema().fields().len(), 1);
        assert_eq!(plan.schema().field(0).name(), "id");
    }

    #[tokio::test]
    async fn wal_scan_pins_ordinals_at_plan_and_ignores_later_segments() {
        use crate::buffer::segment_file::{PartitionKey, WalPayload};
        use crate::buffer::wal_handle::{
            install_wal_handle, memory_catalog_for_test, remove_wal_handle, PipelineWalHandle,
        };
        use crate::helpers::offset_store::{OffsetStore, SledOffsetStore};
        use arrow::array::StringArray;
        use skippr_lease::{LeaseGuard, PipelinePaths, SystemClock};
        use std::collections::HashMap;
        use std::sync::Arc as StdArc;

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let key = PipelineKey::new("t", "w", "wal-pin-plan").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = PipelinePaths::legacy_disk(dir.path());
        let clock = StdArc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.path().join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: StdArc<dyn OffsetStore> = StdArc::new(SledOffsetStore::new(tree));
        let handle = PipelineWalHandle::new(key.clone(), paths.clone(), guard, catalog, offsets);
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
            )
            .await
            .unwrap();
        let provider = WalTableProvider::new(schema.clone(), key.clone(), "ns".into(), Vec::new());
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
    async fn reclaim_between_scan_and_execute_skips_file() {
        use crate::buffer::segment_file::{PartitionKey, WalPayload};
        use crate::buffer::wal_handle::{
            install_wal_handle, memory_catalog_for_test, remove_wal_handle, PipelineWalHandle,
        };
        use crate::helpers::offset_store::{OffsetStore, SledOffsetStore};
        use arrow::array::StringArray;
        use skippr_lease::{LeaseGuard, PipelinePaths, SystemClock};
        use std::collections::HashMap;
        use std::sync::Arc as StdArc;

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let key = PipelineKey::new("t", "w", "wal-reclaim").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = PipelinePaths::legacy_disk(dir.path());
        let clock = StdArc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.path().join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: StdArc<dyn OffsetStore> = StdArc::new(SledOffsetStore::new(tree));
        let handle = PipelineWalHandle::new(key.clone(), paths.clone(), guard, catalog, offsets);
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
            )
            .await
            .unwrap();
        let provider = WalTableProvider::new(schema, key.clone(), "ns".into(), Vec::new());
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        handle
            .complete_slices_and_reclaim("live-seg", &[0])
            .await
            .unwrap();
        let batches =
            datafusion::physical_plan::common::collect(plan.execute(0, ctx.task_ctx()).unwrap())
                .await
                .unwrap();
        remove_wal_handle(&key);
        assert!(batches.iter().all(|batch| batch.num_rows() == 0) || batches.is_empty());
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
            "WalScanExec must use catalog SliceIndexEntry, not FOOT"
        );
        assert!(prod.contains("CatalogKey::Index"));
        assert!(
            prod.contains("wal_slice::read_slice_batches"),
            "query and STREAM share sqlrt::wal_slice"
        );
        assert!(!prod.contains("fn read_batches"));
        assert!(!prod.contains("payload_bytes"));
        assert!(prod.contains("ReadBoundary::pin_wal"));
        assert!(!prod.contains("select_live_ordinals_for(&self.pipeline"));
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
            )
            .await
            .unwrap();
    }

    fn leased_wal(
        dir: &std::path::Path,
        key: &PipelineKey,
    ) -> Arc<crate::buffer::wal_handle::PipelineWalHandle> {
        use crate::buffer::wal_handle::{memory_catalog_for_test, PipelineWalHandle};
        use crate::helpers::offset_store::{OffsetStore, SledOffsetStore};
        use skippr_lease::{LeaseGuard, PipelinePaths, SystemClock};
        use std::sync::Arc as StdArc;

        let paths = PipelinePaths::legacy_disk(dir);
        let clock = StdArc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let fence = crate::buffer::wal_handle::catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: StdArc<dyn OffsetStore> = StdArc::new(SledOffsetStore::new(tree));
        PipelineWalHandle::new(key.clone(), paths, guard, catalog, offsets)
    }

    #[tokio::test]
    async fn cdc_sidecar_loads_from_disk_part_meta() {
        use crate::buffer::wal_handle::{install_wal_handle, remove_wal_handle};
        use crate::plugins::cdc::MutationKind;
        use crate::sqlrt::cdc_reconcile::{CDC_MUTATION_COL, CDC_ORDER_COL};
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
        let provider = WalTableProvider::new(schema, key.clone(), "ns".into(), Vec::new())
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
        let name = batch
            .column_by_name("name")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        assert_eq!(name.value(0), "alpha-prime");
    }

    #[tokio::test]
    async fn mixed_append_and_cdc_skips_excluded_and_unreadable() {
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
            vec!["cdc-excluded".into()],
        )
        .with_cdc_sidecar(true);
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        let batches =
            datafusion::physical_plan::common::collect(plan.execute(0, ctx.task_ctx()).unwrap())
                .await
                .unwrap();
        remove_wal_handle(&key);
        let names: Vec<String> = batches
            .iter()
            .flat_map(|b| {
                b.column_by_name("name")
                    .unwrap()
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap()
                    .iter()
                    .map(|v| v.unwrap().to_string())
            })
            .collect();
        assert!(names.contains(&"prime".to_string()));
        assert!(names.contains(&"extra".to_string()));
        assert!(!names.contains(&"missing".to_string()));
        assert!(!names.contains(&"hidden".to_string()));
    }

    #[tokio::test]
    async fn disk_cdc_merge_replaces_lake_row() {
        use crate::buffer::wal_handle::{install_wal_handle, remove_wal_handle};
        use crate::plugins::cdc::MutationKind;
        use crate::sqlrt::tables::IcebergWalTableProvider;
        use arrow::array::StringArray;
        use datafusion::datasource::MemTable;

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
            WalTableProvider::new(schema.clone(), key.clone(), "ns".into(), Vec::new())
                .with_cdc_sidecar(true),
        );
        let iceberg = Arc::new(MemTable::try_new(schema.clone(), vec![vec![lake]]).unwrap());
        let provider = IcebergWalTableProvider {
            iceberg,
            wal,
            schema: schema.clone(),
            business_key_columns: vec!["id".into()],
        };
        let ctx = datafusion::prelude::SessionContext::new();
        ctx.register_table("t", Arc::new(provider)).unwrap();
        let batches = ctx
            .sql("SELECT name FROM t WHERE id = '1'")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let count = ctx
            .sql("SELECT count(*) FROM t")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        remove_wal_handle(&key);
        let name = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
        assert_eq!(name.value(0), "alpha-prime");
        let n = count[0]
            .column(0)
            .as_any()
            .downcast_ref::<datafusion::arrow::array::Int64Array>()
            .unwrap()
            .value(0);
        assert_eq!(n, 2);
    }
}
