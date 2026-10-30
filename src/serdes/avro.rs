use std::io::Cursor;

use arrow::record_batch::RecordBatch;
use arrow_avro::reader::ReaderBuilder;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AvroDecodeError(String);

pub fn decode_avro(payload: &[u8]) -> Result<Vec<RecordBatch>, AvroDecodeError> {
    let reader = ReaderBuilder::new()
        .build(Cursor::new(payload.to_vec()))
        .map_err(|err| AvroDecodeError(err.to_string()))?;
    reader
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| AvroDecodeError(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::decode_avro;

    #[test]
    fn truncated_avro_fails_closed() {
        let err = decode_avro(b"Obj\x01not-avro").unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
