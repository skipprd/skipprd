use std::any::Any;
use std::fs;
use std::io::{Read, Seek};
use std::sync::Arc;

use arrow::ipc::reader::StreamReader;
use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::logical_expr::Expr;
use datafusion::physical_expr::expressions::col;
use datafusion::physical_plan::empty::EmptyExec;
use datafusion::physical_plan::projection::{ProjectionExec, ProjectionExpr};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties};
use skippr_lease::{PipelineKey, PipelinePaths, SegmentId};

use crate::buffer::segment_file::SegmentFile;
use crate::query_flight::live_wal::select_live_ordinals;

/// Unpinned live WAL provider. DoGet/`execute` re-selects ordinals now.
#[derive(Debug)]
pub struct WalTableProvider {
    pub schema: SchemaRef,
    pub pipeline: PipelineKey,
    pub namespace: String,
    pub exclude_segment_ids: Vec<String>,
    pub live_ordinals: Vec<(String, u32)>,
    pub paths: Option<PipelinePaths>,
}

impl WalTableProvider {
    pub fn live(
        schema: SchemaRef,
        pipeline: PipelineKey,
        namespace: String,
        live_ordinals: Vec<(String, u32)>,
    ) -> Self {
        Self {
            schema,
            pipeline,
            namespace,
            exclude_segment_ids: Vec::new(),
            live_ordinals,
            paths: None,
        }
    }

    pub fn live_unpinned(
        schema: SchemaRef,
        pipeline: PipelineKey,
        namespace: String,
        exclude_segment_ids: Vec<String>,
        paths: PipelinePaths,
    ) -> Self {
        Self {
            schema,
            pipeline,
            namespace,
            exclude_segment_ids,
            live_ordinals: Vec::new(),
            paths: Some(paths),
        }
    }

    pub fn with_paths(mut self, paths: PipelinePaths) -> Self {
        self.paths = Some(paths);
        self
    }

    pub fn with_exclude_segment_ids(mut self, ids: Vec<String>) -> Self {
        self.exclude_segment_ids = ids;
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
    RecordBatch::try_new(schema.clone(), columns)
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
    paths: &PipelinePaths,
    segment_id: &str,
    ordinal: u32,
    namespace: &str,
    schema: &SchemaRef,
) -> Result<Vec<RecordBatch>> {
    let Ok(id) = SegmentId::new(segment_id) else {
        return Ok(Vec::new());
    };
    let path = paths.segment(&id);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => return Ok(Vec::new()),
    };
    let Ok(meta) = SegmentFile::read_metadata_from_reader(&mut file) else {
        return Ok(Vec::new());
    };
    let Some(idx) = meta.index.get(ordinal as usize) else {
        return Ok(Vec::new());
    };
    if idx.key.namespace != namespace {
        return Ok(Vec::new());
    }
    if file.seek(std::io::SeekFrom::Start(idx.start)).is_err() {
        return Ok(Vec::new());
    }
    let reader = std::io::BufReader::new(file);
    let mut take = reader.take(idx.len);
    let sr = match StreamReader::try_new(&mut take, None) {
        Ok(sr) => sr,
        Err(err) => {
            if schema.fields().is_empty() {
                return Ok(Vec::new());
            }
            return Err(DataFusionError::ArrowError(Box::new(err), None));
        }
    };
    let mut out = Vec::new();
    for batch in sr {
        let batch = batch.map_err(|err| DataFusionError::ArrowError(Box::new(err), None))?;
        if schema.fields().is_empty() {
            out.push(batch);
        } else {
            out.push(project_batch(batch, schema)?);
        }
    }
    Ok(out)
}

#[async_trait::async_trait]
impl TableProvider for WalTableProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

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
        let Some(paths) = self.paths.as_ref() else {
            if self.live_ordinals.is_empty() {
                return Ok(Arc::new(EmptyExec::new(schema)));
            }
            return Err(DataFusionError::Plan(
                "clustered WAL scan requires PipelinePaths".into(),
            ));
        };
        Ok(Arc::new(WalScanExec::new(
            schema,
            self.pipeline.clone(),
            self.namespace.clone(),
            self.exclude_segment_ids.clone(),
            paths.clone(),
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
    paths: PipelinePaths,
    limit: Option<usize>,
}

impl WalScanExec {
    pub fn new(
        schema: SchemaRef,
        pipeline: PipelineKey,
        namespace: String,
        exclude_segment_ids: Vec<String>,
        paths: PipelinePaths,
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
            paths,
            limit,
        })
    }

    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn read_batches(&self) -> Result<Vec<RecordBatch>> {
        let log = crate::buffer::durable::log::MutationLog::open(self.paths.clone())
            .map_err(|err| DataFusionError::Execution(err.to_string()))?;
        let ordinals = select_live_ordinals(&log, &self.exclude_segment_ids);
        let mut batches = Vec::new();
        for (segment_id, ordinal) in &ordinals {
            batches.extend(read_ordinal_batches(
                &self.paths,
                segment_id,
                *ordinal,
                &self.namespace,
                &self.schema,
            )?);
        }
        if let Some(limit) = self.limit {
            let mut remaining = limit;
            let mut limited = Vec::new();
            for batch in batches {
                if remaining == 0 {
                    break;
                }
                if batch.num_rows() <= remaining {
                    remaining -= batch.num_rows();
                    limited.push(batch);
                } else {
                    limited.push(batch.slice(0, remaining));
                    break;
                }
            }
            batches = limited;
        }
        Ok(batches)
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

    fn as_any(&self) -> &dyn Any {
        self
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

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let batches = self.read_batches()?;
        let stream = futures::stream::iter(batches.into_iter().map(Ok));
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            self.schema.clone(),
            stream,
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
    fn provider_scans_selected_ordinals_only() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let key = PipelineKey::new("t", "w", "events").unwrap();
        let provider = WalTableProvider::live(
            schema,
            key,
            "ns".into(),
            vec![("seg-a".into(), 0), ("seg-a".into(), 2)],
        );
        assert_eq!(provider.namespace, "ns");
        assert_eq!(
            provider.live_ordinals,
            vec![("seg-a".to_string(), 0), ("seg-a".to_string(), 2)]
        );
    }

    #[tokio::test]
    async fn missing_segment_file_skips_ordinal() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let key = PipelineKey::new("t", "w", "events").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        std::fs::create_dir_all(&paths.segs).unwrap();
        let provider = WalTableProvider::live(
            schema.clone(),
            key,
            "ns".into(),
            vec![("gone-seg".into(), 0)],
        )
        .with_paths(paths);
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
        let provider = WalTableProvider::live(schema, key, "ns".into(), vec![]);
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
        let dir = tempfile::tempdir().unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        std::fs::create_dir_all(&paths.segs).unwrap();
        let provider =
            WalTableProvider::live_unpinned(schema, key, "ns".into(), vec!["gone".into()], paths);
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
        use crate::buffer::durable::mutation::{
            DurableMutation, MutationEnvelope, SegmentDescriptor,
        };
        use crate::buffer::segment_file::{PartitionKey, SegmentFile};
        use arrow::array::StringArray;
        use skippr_lease::{CommitIndex, GENESIS_HASH};
        use std::collections::HashMap;

        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let key = PipelineKey::new("t", "w", "events").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        std::fs::create_dir_all(&paths.segs).unwrap();
        let seg = SegmentFile::new(&paths.segs, "live-seg").unwrap();
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
            vec![RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(StringArray::from(vec!["evt-a"]))],
            )
            .unwrap()],
        );
        seg.write_snapshot(&HashMap::new(), &batches, &HashMap::new(), &HashMap::new())
            .unwrap();
        let mut log = crate::buffer::durable::log::MutationLog::open(paths.clone()).unwrap();
        let commit = MutationEnvelope {
            protocol_version: 1,
            pipeline: key.clone(),
            epoch: skippr_lease::LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [9u8; 32],
            body: DurableMutation::CommitSegment {
                descriptor: SegmentDescriptor {
                    segment_id: "live-seg".into(),
                    payload_len: 4,
                    payload_sha256: [9u8; 32],
                    num_partitions: 1,
                    total_bytes: 4,
                    created_at_secs: 0,
                    schema_fingerprints: Vec::new(),
                },
                offsets: Vec::new(),
                checkpoints: Vec::new(),
            },
        };
        log.append_prepared(&commit).unwrap();
        log.append_committed(commit.index, commit.entry_hash().unwrap())
            .unwrap();
        let provider =
            WalTableProvider::live_unpinned(schema, key.clone(), "ns".into(), Vec::new(), paths);
        let ctx = datafusion::prelude::SessionContext::new();
        let plan = provider.scan(&ctx.state(), None, &[], None).await.unwrap();
        let reclaim = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: skippr_lease::LeaseEpoch::new(1),
            index: CommitIndex::new(2),
            previous_hash: commit.entry_hash().unwrap(),
            payload_sha256: [0u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "live-seg".into(),
            },
        };
        log.append_prepared(&reclaim).unwrap();
        log.append_committed(reclaim.index, reclaim.entry_hash().unwrap())
            .unwrap();
        let batches =
            datafusion::physical_plan::common::collect(plan.execute(0, ctx.task_ctx()).unwrap())
                .await
                .unwrap();
        assert!(batches.iter().all(|batch| batch.num_rows() == 0) || batches.is_empty());
    }
}
