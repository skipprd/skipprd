use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll as TaskPoll};

use arrow::array::{Int32Array, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::error::DataFusionError;
use datafusion::physical_plan::RecordBatchStream;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::Stream;

use skippr_runtime_sdk::plugins::cdc::{MutationKind, SyncContext, WalPartMeta};

/// Augment a RecordBatch stream with CDC metadata columns.
/// Returns a new stream that adds mutation, order token, and tie columns
/// for every change. Does not call collapse.
pub fn augment_cdc_stream(
    stream: SendableRecordBatchStream,
    ctx: &SyncContext,
) -> SendableRecordBatchStream {
    augment_stream_with_cdc_columns(stream, &ctx.part_meta, &ctx.ties)
}

pub fn augment_stream_with_cdc_columns(
    stream: SendableRecordBatchStream,
    part_meta: &WalPartMeta,
    ties: &[(u64, u32, u64)],
) -> SendableRecordBatchStream {
    let orig_schema = stream.schema();
    let augmented_schema = build_augmented_schema(&orig_schema);
    Box::pin(CdcEncodeStream {
        inner: stream,
        schema: augmented_schema,
        rows: part_meta.rows.clone(),
        ties: ties.to_vec(),
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
    fields.push(Arc::new(Field::new(
        "_skippr_tie_seq",
        DataType::Int64,
        false,
    )));
    fields.push(Arc::new(Field::new(
        "_skippr_tie_ordinal",
        DataType::Int32,
        false,
    )));
    fields.push(Arc::new(Field::new(
        "_skippr_tie_row",
        DataType::Int64,
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
    ties: Vec<(u64, u32, u64)>,
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

                if end > this.ties.len() {
                    return TaskPoll::Ready(Some(Err(DataFusionError::Internal(format!(
                        "CDC ties have {} entries but stream reached row offset {}",
                        this.ties.len(),
                        end,
                    )))));
                }
                let order_tokens: StringArray = slice
                    .iter()
                    .map(|r| Some(hex::encode(&r.order_token)))
                    .collect();
                let tie_slice = &this.ties[start..end];
                let tie_seq: Int64Array = tie_slice.iter().map(|tie| Some(tie.0 as i64)).collect();
                let tie_ord: Int32Array = tie_slice.iter().map(|tie| Some(tie.1 as i32)).collect();
                let tie_row: Int64Array = tie_slice.iter().map(|tie| Some(tie.2 as i64)).collect();

                let mut columns: Vec<Arc<dyn arrow::array::Array>> = batch.columns().to_vec();
                columns.push(Arc::new(mutations));
                columns.push(Arc::new(order_tokens));
                columns.push(Arc::new(tie_seq));
                columns.push(Arc::new(tie_ord));
                columns.push(Arc::new(tie_row));

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
    use arrow::array::{Int32Array, Int64Array, StringArray as ArrowStringArray};
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
                skippr_runtime_sdk::plugins::cdc::WalRowMeta::new(
                    MutationKind::Insert,
                    b"ev1".to_vec(),
                    vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01],
                ),
                skippr_runtime_sdk::plugins::cdc::WalRowMeta::new(
                    MutationKind::Update,
                    b"ev2".to_vec(),
                    vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02],
                ),
                skippr_runtime_sdk::plugins::cdc::WalRowMeta::new(
                    MutationKind::Delete,
                    b"ev3".to_vec(),
                    vec![0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89],
                ),
            ],
        }
    }

    #[tokio::test]
    async fn test_augment_stream_adds_cdc_columns() {
        let batch = make_test_batch();
        let stream = make_test_stream(batch);

        let meta = make_test_wal_meta();
        let ties: Vec<(u64, u32, u64)> = (0..3).map(|i| (1, 0, i)).collect();
        let mut augmented = augment_stream_with_cdc_columns(stream, &meta, &ties);

        let schema = augmented.schema();
        assert_eq!(schema.fields().len(), 7);
        assert_eq!(schema.field(2).name(), "_skippr_mutation");
        assert_eq!(schema.field(2).data_type(), &DataType::Utf8);
        assert_eq!(schema.field(3).name(), "_skippr_order_token");
        assert_eq!(schema.field(3).data_type(), &DataType::Utf8);
        assert_eq!(schema.field(4).name(), "_skippr_tie_seq");
        assert_eq!(schema.field(5).name(), "_skippr_tie_ordinal");
        assert_eq!(schema.field(6).name(), "_skippr_tie_row");

        let result_batch = augmented.next().await.unwrap().unwrap();
        assert_eq!(result_batch.num_rows(), 3);
        assert_eq!(result_batch.num_columns(), 7);

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

    #[tokio::test]
    async fn cdc_metadata_remains_aligned_across_streamed_chunks() {
        let batch = make_test_batch();
        let schema = batch.schema();
        let stream: SendableRecordBatchStream = Box::pin(VecBatchStream {
            schema,
            batches: vec![batch.slice(0, 1), batch.slice(1, 2)],
            index: 0,
        });
        let ties: Vec<(u64, u32, u64)> = (0..3).map(|i| (1, 0, i)).collect();
        let mut augmented = augment_stream_with_cdc_columns(stream, &make_test_wal_meta(), &ties);

        let first = augmented.next().await.unwrap().unwrap();
        let second = augmented.next().await.unwrap().unwrap();
        let first_tokens = first
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let second_tokens = second
            .column(3)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(first_tokens.value(0), "0000000000000001");
        assert_eq!(second_tokens.value(0), "0000000000000002");
        assert_eq!(second_tokens.value(1), "abcdef0123456789");
        assert!(augmented.next().await.is_none());
    }

    #[test]
    fn test_build_augmented_schema() {
        let orig = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let augmented = build_augmented_schema(&orig);
        assert_eq!(augmented.fields().len(), 6);
        assert_eq!(augmented.field(1).name(), "_skippr_mutation");
        assert_eq!(augmented.field(2).name(), "_skippr_order_token");
        assert_eq!(augmented.field(3).name(), "_skippr_tie_seq");
        assert_eq!(augmented.field(4).name(), "_skippr_tie_ordinal");
        assert_eq!(augmented.field(5).name(), "_skippr_tie_row");
    }

    #[test]
    fn test_mutation_kind_str_all_variants() {
        assert_eq!(mutation_kind_str(&MutationKind::Snapshot), "snapshot");
        assert_eq!(mutation_kind_str(&MutationKind::Insert), "insert");
        assert_eq!(mutation_kind_str(&MutationKind::Update), "update");
        assert_eq!(mutation_kind_str(&MutationKind::Delete), "delete");
    }

    #[tokio::test]
    async fn two_changes_of_one_key_write_two_encoded_rows_with_tie() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from(vec![1i64, 1])) as Arc<dyn arrow::array::Array>],
        )
        .unwrap();
        let stream = Box::pin(VecBatchStream {
            schema,
            batches: vec![batch],
            index: 0,
        }) as SendableRecordBatchStream;
        let first = skippr_runtime_sdk::plugins::cdc::WalRowMeta::new(
            MutationKind::Insert,
            b"e1".to_vec(),
            vec![1],
        );
        let second = skippr_runtime_sdk::plugins::cdc::WalRowMeta::new(
            MutationKind::Update,
            b"e2".to_vec(),
            vec![1],
        );
        let meta = WalPartMeta {
            kind: skippr_runtime_sdk::plugins::cdc::WalPartKind::Cdc,
            row_count: 2,
            rows: vec![first, second],
        };
        let ties = vec![(9, 2, 0), (9, 2, 1)];
        let mut augmented = augment_stream_with_cdc_columns(stream, &meta, &ties);
        let result = augmented.next().await.unwrap().unwrap();
        assert_eq!(result.num_rows(), 2);
        let seq = result
            .column_by_name("_skippr_tie_seq")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(seq.value(0), 9);
        assert_eq!(seq.value(1), 9);
        let ord = result
            .column_by_name("_skippr_tie_ordinal")
            .unwrap()
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(ord.value(0), 2);
        assert_eq!(ord.value(1), 2);
        let row = result
            .column_by_name("_skippr_tie_row")
            .unwrap()
            .as_any()
            .downcast_ref::<Int64Array>()
            .unwrap();
        assert_eq!(row.value(0), 0);
        assert_eq!(row.value(1), 1);
    }

    #[tokio::test]
    async fn empty_order_token_encodes_to_empty_hex() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from(vec![1i64])) as Arc<dyn arrow::array::Array>],
        )
        .unwrap();
        let stream = Box::pin(VecBatchStream {
            schema,
            batches: vec![batch],
            index: 0,
        }) as SendableRecordBatchStream;
        let meta = WalPartMeta {
            kind: skippr_runtime_sdk::plugins::cdc::WalPartKind::Cdc,
            row_count: 1,
            rows: vec![skippr_runtime_sdk::plugins::cdc::WalRowMeta::new(
                MutationKind::Snapshot,
                b"snap".to_vec(),
                vec![],
            )],
        };
        let mut augmented = augment_stream_with_cdc_columns(stream, &meta, &[(1, 0, 0)]);
        let result = augmented.next().await.unwrap().unwrap();
        let tokens = result
            .column_by_name("_skippr_order_token")
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(tokens.value(0), "");
    }
}
