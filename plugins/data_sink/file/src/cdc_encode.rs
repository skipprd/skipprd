use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll as TaskPoll};

use arrow::array::{Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::error::DataFusionError;
use datafusion::physical_plan::{RecordBatchStream, SendableRecordBatchStream};
use futures::Stream;
use skippr::plugins::cdc::{MutationKind, WalPartMeta, WalRowMeta};

pub fn augment_stream_with_cdc_columns(
    stream: SendableRecordBatchStream,
    part_meta: &WalPartMeta,
) -> SendableRecordBatchStream {
    let orig_schema = stream.schema();
    let augmented_schema = build_augmented_schema(&orig_schema);
    Box::pin(CdcEncodeStream {
        inner: stream,
        schema: augmented_schema,
        rows: part_meta.rows.clone(),
        row_offset: 0,
    })
}

fn build_augmented_schema(original: &SchemaRef) -> SchemaRef {
    let mut fields: Vec<Arc<Field>> = original.fields().iter().cloned().collect();
    fields.push(Arc::new(Field::new(
        "_skippr_mutation",
        DataType::Utf8,
        false,
    )));
    fields.push(Arc::new(Field::new(
        "_skippr_order_token",
        DataType::Utf8,
        false,
    )));
    Arc::new(Schema::new(fields))
}

fn mutation_kind_str(kind: &MutationKind) -> &'static str {
    match kind {
        MutationKind::Snapshot => "snapshot",
        MutationKind::Insert => "insert",
        MutationKind::Update => "update",
        MutationKind::Delete => "delete",
    }
}

struct CdcEncodeStream {
    inner: SendableRecordBatchStream,
    schema: SchemaRef,
    rows: Vec<WalRowMeta>,
    row_offset: usize,
}

impl Stream for CdcEncodeStream {
    type Item = Result<RecordBatch, DataFusionError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> TaskPoll<Option<Self::Item>> {
        let this = unsafe { self.get_unchecked_mut() };
        match Pin::new(&mut this.inner).poll_next(cx) {
            TaskPoll::Ready(Some(Ok(batch))) => {
                let num_rows = batch.num_rows();
                let start = this.row_offset;
                let end = start + num_rows;

                if end > this.rows.len() {
                    return TaskPoll::Ready(Some(Err(DataFusionError::Internal(format!(
                        "CDC row metadata has {} entries but stream reached row offset {}",
                        this.rows.len(),
                        end,
                    )))));
                }

                let slice = &this.rows[start..end];
                this.row_offset = end;

                let mutations: StringArray = slice
                    .iter()
                    .map(|row| Some(mutation_kind_str(&row.mutation)))
                    .collect();
                let order_tokens: StringArray = slice
                    .iter()
                    .map(|row| Some(hex::encode(&row.order_token)))
                    .collect();

                let mut columns: Vec<Arc<dyn Array>> = batch.columns().to_vec();
                columns.push(Arc::new(mutations));
                columns.push(Arc::new(order_tokens));

                match RecordBatch::try_new(this.schema.clone(), columns) {
                    Ok(augmented) => TaskPoll::Ready(Some(Ok(augmented))),
                    Err(err) => {
                        TaskPoll::Ready(Some(Err(DataFusionError::ArrowError(Box::new(err), None))))
                    }
                }
            }
            TaskPoll::Ready(Some(Err(err))) => TaskPoll::Ready(Some(Err(err))),
            TaskPoll::Ready(None) => TaskPoll::Ready(None),
            TaskPoll::Pending => TaskPoll::Pending,
        }
    }
}

impl RecordBatchStream for CdcEncodeStream {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}
