use std::any::Any;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use datafusion::arrow::array::RecordBatch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties};
use futures::Stream;

pub type BatchStream = Pin<Box<dyn Stream<Item = Result<RecordBatch>> + Send>>;

pub struct StreamingBatchesExec {
    schema: SchemaRef,
    properties: Arc<PlanProperties>,
    stream: Mutex<Option<BatchStream>>,
}

impl std::fmt::Debug for StreamingBatchesExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingBatchesExec").finish()
    }
}

impl StreamingBatchesExec {
    pub fn new(schema: SchemaRef, stream: BatchStream) -> Self {
        let properties = Arc::new(PlanProperties::new(
            datafusion::physical_expr::EquivalenceProperties::new(schema.clone()),
            datafusion::physical_plan::Partitioning::UnknownPartitioning(1),
            datafusion::physical_plan::execution_plan::EmissionType::Incremental,
            datafusion::physical_plan::execution_plan::Boundedness::Bounded,
        ));
        Self {
            schema,
            properties,
            stream: Mutex::new(Some(stream)),
        }
    }

    pub fn from_batches(schema: SchemaRef, batches: Vec<RecordBatch>) -> Self {
        let stream = futures::stream::iter(batches.into_iter().map(Ok));
        Self::new(schema, Box::pin(stream))
    }
}

impl DisplayAs for StreamingBatchesExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "StreamingBatchesExec")
    }
}

impl ExecutionPlan for StreamingBatchesExec {
    fn name(&self) -> &str {
        "StreamingBatchesExec"
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
        let stream = self
            .stream
            .lock()
            .expect("stream mutex")
            .take()
            .ok_or_else(|| DataFusionError::Internal("stream already consumed".into()))?;
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            self.schema.clone(),
            stream,
        )))
    }
}
