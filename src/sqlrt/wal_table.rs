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
use crate::query_flight::live_wal::select_live_ordinals_for;
use crate::sqlrt::wal_slice;

/// Unpinned live WAL provider. DoGet/`execute` re-selects ordinals now.
#[derive(Debug)]
pub struct WalTableProvider {
    pub schema: SchemaRef,
    pub pipeline: PipelineKey,
    pub namespace: String,
    pub exclude_segment_ids: Vec<String>,
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
        }
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
) -> Result<Vec<RecordBatch>> {
    let Some(handle) = wal_handle::wal_handle_for(pipeline) else {
        return Ok(Vec::new());
    };
    let Ok(id) = SegmentId::new(segment_id) else {
        return Ok(Vec::new());
    };
    let Ok(Some(CatalogRow::Slice(slice))) = handle.catalog().get(&CatalogKey::Slice {
        segment_id: id.clone(),
        ordinal,
    }) else {
        return Ok(Vec::new());
    };
    if slice.namespace != namespace {
        return Ok(Vec::new());
    }
    let Ok(Some(CatalogRow::Segment(seg))) = handle.catalog().get(&CatalogKey::Segment(id)) else {
        return Ok(Vec::new());
    };
    let batches = wal_slice::read_slice_batches(&seg.location, &slice)
        .map_err(|err| DataFusionError::Execution(err.to_string()))?;
    if schema.fields().is_empty() {
        return Ok(batches);
    }
    batches
        .into_iter()
        .map(|batch| project_batch(batch, schema))
        .collect()
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
        let schema = projected_schema(&self.schema, projection)?;
        Ok(Arc::new(WalScanExec::new(
            schema,
            self.pipeline.clone(),
            self.namespace.clone(),
            self.exclude_segment_ids.clone(),
            limit,
        )?))
    }
}

/// Physical WAL leaf. Re-selects live ordinals in `execute`, not at plan time.
#[derive(Debug, Clone)]
pub struct WalScanExec {
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
    pipeline: PipelineKey,
    namespace: String,
    exclude_segment_ids: Vec<String>,
    limit: Option<usize>,
}

impl WalScanExec {
    pub fn new(
        schema: SchemaRef,
        pipeline: PipelineKey,
        namespace: String,
        exclude_segment_ids: Vec<String>,
        limit: Option<usize>,
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
            exclude_segment_ids,
            limit,
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
        let ordinals = select_live_ordinals_for(&self.pipeline, &self.exclude_segment_ids);
        let pipeline = self.pipeline.clone();
        let namespace = self.namespace.clone();
        let schema = self.schema.clone();
        let mut remaining = self.limit;
        let mut out = Vec::new();
        for (segment_id, ordinal) in ordinals {
            if remaining == Some(0) {
                break;
            }
            for batch in read_ordinal_batches(&pipeline, &segment_id, ordinal, &namespace, &schema)?
            {
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
        let key = PipelineKey::new("t", "w", "events").unwrap();
        let provider = WalTableProvider::new(schema, key, "ns".into(), vec!["gone".into()]);
        assert_eq!(provider.namespace, "ns");
        assert_eq!(provider.exclude_segment_ids, vec!["gone".to_string()]);
    }

    #[tokio::test]
    async fn missing_segment_file_skips_ordinal() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let key = PipelineKey::new("t", "w", "events").unwrap();
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
        let key = PipelineKey::new("t", "w", "events").unwrap();
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
    async fn unpinned_wal_scan_reselects_ordinals_at_execute() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let key = PipelineKey::new("t", "w", "events").unwrap();
        let provider = WalTableProvider::new(schema, key, "ns".into(), vec!["gone".into()]);
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        assert_eq!(plan.name(), "WalScanExec");
        let batches =
            datafusion::physical_plan::common::collect(plan.execute(0, ctx.task_ctx()).unwrap())
                .await
                .unwrap();
        assert!(batches.iter().all(|batch| batch.num_rows() == 0) || batches.is_empty());
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
        let key = PipelineKey::new("t", "w", "events").unwrap();
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
        assert!(prod.contains("CatalogKey::Slice"));
        assert!(
            prod.contains("wal_slice::read_slice_batches"),
            "query and STREAM share sqlrt::wal_slice"
        );
        assert!(!prod.contains("fn read_batches"));
        assert!(!prod.contains("payload_bytes"));
    }
}
