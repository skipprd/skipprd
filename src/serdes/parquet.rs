use std::sync::Arc;

use arrow::record_batch::RecordBatch;
use bytes::Bytes;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::errors::ParquetError;

pub const PARQUET_ARROW_ROWS: usize = 16_384;

pub fn decode_parquet(payload: &[u8]) -> Result<Vec<RecordBatch>, ParquetError> {
    let bytes = Bytes::copy_from_slice(payload);
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(bytes)?.with_batch_size(PARQUET_ARROW_ROWS);
    let reader = builder.build()?;
    reader
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| ParquetError::General(err.to_string()))
}

pub fn encode_parquet(batches: &[RecordBatch]) -> Result<Vec<u8>, ParquetError> {
    use parquet::arrow::arrow_writer::ArrowWriter;
    let schema = batches
        .first()
        .map(|batch| batch.schema())
        .unwrap_or_else(|| Arc::new(arrow::datatypes::Schema::empty()));
    let mut buf = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut buf, schema, None)?;
    for batch in batches {
        writer.write(batch)?;
    }
    writer.close()?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Decimal128Array, Int64Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    #[test]
    fn round_trip_int64() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vec![1i64, 2, 3]))])
                .unwrap();
        let bytes = encode_parquet(&[batch]).unwrap();
        let decoded = decode_parquet(&bytes).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].num_rows(), 3);
    }

    #[test]
    fn round_trip_decimal_15_2() {
        let arr = Decimal128Array::from(vec![Some(12345i128)])
            .with_precision_and_scale(15, 2)
            .unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "price",
            DataType::Decimal128(15, 2),
            true,
        )]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(arr)]).unwrap();
        let bytes = encode_parquet(&[batch]).unwrap();
        let decoded = decode_parquet(&bytes).unwrap();
        let col = decoded[0]
            .column(0)
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(col.value(0), 12345);
        assert_eq!(col.scale(), 2);
    }

    #[test]
    fn truncated_payload_fails() {
        let err = decode_parquet(b"PAR1not-a-file").unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
