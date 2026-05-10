use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll as TaskPoll};

use arrow::array::{RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::error::DataFusionError;
use datafusion::physical_plan::RecordBatchStream;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::Stream;

use skippr_runtime_sdk::plugins::cdc::{MutationKind, WalPartMeta};

/// Augment a RecordBatch stream with CDC metadata columns.
/// Returns a new stream that adds `_skippr_mutation` and `_skippr_order_token`
/// columns to each batch based on the aligned WalRowMeta entries.
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
    rows: Vec<skippr_runtime_sdk::plugins::cdc::WalRowMeta>,
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
                    .map(|r| Some(mutation_kind_str(&r.mutation)))
                    .collect();

                let order_tokens: StringArray = slice
                    .iter()
                    .map(|r| Some(hex::encode(&r.order_token)))
                    .collect();

                let mut columns: Vec<Arc<dyn arrow::array::Array>> = batch.columns().to_vec();
                columns.push(Arc::new(mutations));
                columns.push(Arc::new(order_tokens));

                match RecordBatch::try_new(this.schema.clone(), columns) {
                    Ok(augmented) => TaskPoll::Ready(Some(Ok(augmented))),
                    Err(e) => {
                        TaskPoll::Ready(Some(Err(DataFusionError::ArrowError(Box::new(e), None))))
                    }
                }
            }
            TaskPoll::Ready(Some(Err(e))) => TaskPoll::Ready(Some(Err(e))),
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

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray as ArrowStringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use futures::StreamExt;

    struct VecBatchStream {
        schema: SchemaRef,
        batches: Vec<RecordBatch>,
        index: usize,
    }

    impl Stream for VecBatchStream {
        type Item = Result<RecordBatch, DataFusionError>;
        fn poll_next(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
        ) -> TaskPoll<Option<Self::Item>> {
            let this = unsafe { self.get_unchecked_mut() };
            if this.index < this.batches.len() {
                let batch = this.batches[this.index].clone();
                this.index += 1;
                TaskPoll::Ready(Some(Ok(batch)))
            } else {
                TaskPoll::Ready(None)
            }
        }
    }

    impl RecordBatchStream for VecBatchStream {
        fn schema(&self) -> SchemaRef {
            self.schema.clone()
        }
    }

    fn make_test_batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let ids = Int64Array::from(vec![1, 2, 3]);
        let names = ArrowStringArray::from(vec![Some("Alice"), Some("Bob"), Some("Charlie")]);
        RecordBatch::try_new(schema, vec![Arc::new(ids), Arc::new(names)]).unwrap()
    }

    fn make_test_stream(batch: RecordBatch) -> SendableRecordBatchStream {
        let schema = batch.schema();
        Box::pin(VecBatchStream {
            schema,
            batches: vec![batch],
            index: 0,
        })
    }

    fn make_test_wal_meta() -> WalPartMeta {
        WalPartMeta {
            kind: skippr_runtime_sdk::plugins::cdc::WalPartKind::Cdc,
            row_count: 3,
            rows: vec![
                skippr_runtime_sdk::plugins::cdc::WalRowMeta {
                    mutation: MutationKind::Insert,
                    event_id: b"ev1".to_vec(),
                    order_token: vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01],
                },
                skippr_runtime_sdk::plugins::cdc::WalRowMeta {
                    mutation: MutationKind::Update,
                    event_id: b"ev2".to_vec(),
                    order_token: vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02],
                },
                skippr_runtime_sdk::plugins::cdc::WalRowMeta {
                    mutation: MutationKind::Delete,
                    event_id: b"ev3".to_vec(),
                    order_token: vec![0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89],
                },
            ],
        }
    }

    #[tokio::test]
    async fn test_augment_stream_adds_cdc_columns() {
        let batch = make_test_batch();
        let stream = make_test_stream(batch);

        let meta = make_test_wal_meta();
        let mut augmented = augment_stream_with_cdc_columns(stream, &meta);

        let schema = augmented.schema();
        assert_eq!(schema.fields().len(), 4);
        assert_eq!(schema.field(2).name(), "_skippr_mutation");
        assert_eq!(schema.field(2).data_type(), &DataType::Utf8);
        assert_eq!(schema.field(3).name(), "_skippr_order_token");
        assert_eq!(schema.field(3).data_type(), &DataType::Utf8);

        let result_batch = augmented.next().await.unwrap().unwrap();
        assert_eq!(result_batch.num_rows(), 3);
        assert_eq!(result_batch.num_columns(), 4);

        let mutations = result_batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(mutations.value(0), "insert");
        assert_eq!(mutations.value(1), "update");
        assert_eq!(mutations.value(2), "delete");

        let tokens = result_batch
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(tokens.value(0), "0000000000000001");
        assert_eq!(tokens.value(1), "0000000000000002");
        assert_eq!(tokens.value(2), "abcdef0123456789");
    }

    #[test]
    fn test_build_augmented_schema() {
        let orig = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let augmented = build_augmented_schema(&orig);
        assert_eq!(augmented.fields().len(), 3);
        assert_eq!(augmented.field(1).name(), "_skippr_mutation");
        assert_eq!(augmented.field(2).name(), "_skippr_order_token");
    }

    #[test]
    fn test_mutation_kind_str_all_variants() {
        assert_eq!(mutation_kind_str(&MutationKind::Snapshot), "snapshot");
        assert_eq!(mutation_kind_str(&MutationKind::Insert), "insert");
        assert_eq!(mutation_kind_str(&MutationKind::Update), "update");
        assert_eq!(mutation_kind_str(&MutationKind::Delete), "delete");
    }
}
