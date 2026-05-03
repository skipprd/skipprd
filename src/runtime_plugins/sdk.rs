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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedRecordBatchStream {
    pub bytes: Vec<u8>,
    pub rows: u64,
}

pub struct DecodedRecordBatchStream {
    pub stream: SendableRecordBatchStream,
    pub rows: u64,
}

pub async fn encode_record_batch_stream(
    stream: SendableRecordBatchStream,
) -> Result<Vec<u8>, io::Error> {
    encode_record_batch_stream_with_stats(stream)
        .await
        .map(|encoded| encoded.bytes)
}

pub async fn encode_record_batch_stream_with_stats(
    mut stream: SendableRecordBatchStream,
) -> Result<EncodedRecordBatchStream, io::Error> {
    let mut batches = Vec::new();
    let mut rows = 0u64;
    while let Some(batch_result) = stream.next().await {
        let batch = batch_result.map_err(|err| io::Error::other(err.to_string()))?;
        rows = rows.saturating_add(batch.num_rows() as u64);
        batches.push(batch);
    }
    let bytes = encode_record_batches(&batches)?;
    Ok(EncodedRecordBatchStream { bytes, rows })
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
    decode_record_batch_stream_with_stats(bytes).map(|decoded| decoded.stream)
}

pub fn decode_record_batch_stream_with_stats(
    bytes: Vec<u8>,
) -> Result<DecodedRecordBatchStream, io::Error> {
    let reader = StreamReader::try_new(Cursor::new(bytes), None)
        .map_err(|err| io::Error::other(err.to_string()))?;
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| io::Error::other(err.to_string()))?;
    let rows = batches.iter().fold(0u64, |rows, batch| {
        rows.saturating_add(batch.num_rows() as u64)
    });
    Ok(DecodedRecordBatchStream {
        stream: Box::pin(VecRecordBatchStream::new(schema, batches)),
        rows,
    })
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

    #[tokio::test]
    async fn encoded_stream_reports_total_rows() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch_one = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from(vec![1, 2, 3]))],
        )
        .unwrap();
        let batch_two =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![4]))])
                .unwrap();

        let stream: SendableRecordBatchStream = Box::pin(VecRecordBatchStream::new(
            schema,
            vec![batch_one, batch_two],
        ));

        let encoded = encode_record_batch_stream_with_stats(stream).await.unwrap();

        assert_eq!(encoded.rows, 4);
        assert!(!encoded.bytes.is_empty());
    }

    #[tokio::test]
    async fn decoded_stream_reports_total_rows() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from(vec![1, 2, 3]))],
        )
        .unwrap();
        let bytes = encode_record_batches(&[batch]).unwrap();

        let decoded = decode_record_batch_stream_with_stats(bytes).unwrap();

        assert_eq!(decoded.rows, 3);
    }
}
