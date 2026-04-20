use std::io;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow::record_batch::RecordBatch;
use arrow_schema::SchemaRef;
use datafusion::error::DataFusionError;
use datafusion::physical_plan::{RecordBatchStream, SendableRecordBatchStream};
use futures::Stream;
use futures::StreamExt;

pub async fn encode_record_batch_stream(
    mut stream: SendableRecordBatchStream,
) -> Result<Vec<u8>, io::Error> {
    let mut batches = Vec::new();
    while let Some(batch_result) = stream.next().await {
        batches.push(batch_result.map_err(|err| io::Error::other(err.to_string()))?);
    }
    encode_record_batches(&batches)
}

pub fn encode_record_batches(batches: &[RecordBatch]) -> Result<Vec<u8>, io::Error> {
    let schema = batches
        .first()
        .map(|batch| batch.schema())
        .unwrap_or_else(|| Arc::new(arrow_schema::Schema::empty()));
    let mut bytes = Vec::new();
    let options = IpcWriteOptions::default();
    let mut writer = StreamWriter::try_new_with_options(&mut bytes, &schema, options)
        .map_err(|err| io::Error::other(err.to_string()))?;
    for batch in batches {
        writer
            .write(batch)
            .map_err(|err| io::Error::other(err.to_string()))?;
    }
    writer
        .finish()
        .map_err(|err| io::Error::other(err.to_string()))?;
    Ok(bytes)
}

pub fn decode_record_batch_stream(bytes: Vec<u8>) -> Result<SendableRecordBatchStream, io::Error> {
    let reader = StreamReader::try_new(Cursor::new(bytes), None)
        .map_err(|err| io::Error::other(err.to_string()))?;
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| io::Error::other(err.to_string()))?;
    Ok(Box::pin(VecRecordBatchStream::new(schema, batches)))
}

struct VecRecordBatchStream {
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    idx: usize,
}

impl VecRecordBatchStream {
    fn new(schema: SchemaRef, batches: Vec<RecordBatch>) -> Self {
        Self {
            schema,
            batches,
            idx: 0,
        }
    }
}

impl Stream for VecRecordBatchStream {
    type Item = Result<RecordBatch, DataFusionError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Some(batch) = this.batches.get(this.idx).cloned() {
            this.idx += 1;
            Poll::Ready(Some(Ok(batch)))
        } else {
            Poll::Ready(None)
        }
    }
}

impl RecordBatchStream for VecRecordBatchStream {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};

    #[tokio::test]
    async fn arrow_stream_roundtrips_through_runtime_sdk() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec!["alice", "bob"])),
            ],
        )
        .unwrap();

        let stream: SendableRecordBatchStream =
            Box::pin(VecRecordBatchStream::new(schema, vec![batch.clone()]));

        let bytes = encode_record_batch_stream(stream).await.unwrap();
        let mut decoded = decode_record_batch_stream(bytes).unwrap();
        let decoded_batch = decoded.next().await.unwrap().unwrap();

        assert_eq!(decoded_batch.num_rows(), batch.num_rows());
        assert_eq!(decoded_batch.num_columns(), batch.num_columns());
        assert_eq!(decoded_batch.schema(), batch.schema());
    }
}
