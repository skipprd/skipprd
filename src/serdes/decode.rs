use serde_json::Value;
use thiserror::Error;

use arrow::record_batch::RecordBatch;

use crate::helpers::configuration::Config;
use crate::serdes::csv::{CsvDecodeError, SerderCsv};
use crate::serdes::input_format::InputFormat;
use crate::serdes::json::SerdeJson;
use crate::serdes::xml::{SerdeXml, XmlDecodeError};

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("{0}")]
    Unsupported(String),
    #[error("payload is not valid UTF-8 for {0} input")]
    Utf8(&'static str),
    #[error(transparent)]
    Csv(#[from] CsvDecodeError),
    #[error(transparent)]
    Xml(#[from] XmlDecodeError),
    #[error("{0}")]
    Arrow(String),
}

#[derive(Debug)]
pub enum DecodedIngest {
    Rows(Vec<Value>),
    Arrow(Vec<RecordBatch>),
}

pub fn decode_ingest(
    config: &Config,
    format: InputFormat,
    payload: &[u8],
) -> Result<DecodedIngest, DecodeError> {
    match format {
        InputFormat::Json | InputFormat::Csv | InputFormat::Xml => {
            let text =
                std::str::from_utf8(payload).map_err(|_| DecodeError::Utf8(format.as_str()))?;
            Ok(DecodedIngest::Rows(decode_text_records(
                config, format, text,
            )?))
        }
        InputFormat::Parquet => crate::serdes::parquet::decode_parquet(payload)
            .map(DecodedIngest::Arrow)
            .map_err(|err| DecodeError::Arrow(err.to_string())),
        InputFormat::ArrowIpc => decode_arrow_ipc(payload).map(DecodedIngest::Arrow),
        InputFormat::Avro => crate::serdes::avro::decode_avro(payload)
            .map(DecodedIngest::Arrow)
            .map_err(|err| DecodeError::Arrow(err.to_string())),
    }
}

fn decode_text_records(
    config: &Config,
    format: InputFormat,
    payload: &str,
) -> Result<Vec<Value>, DecodeError> {
    match format {
        InputFormat::Csv => SerderCsv::deserialize(payload).map_err(DecodeError::from),
        InputFormat::Xml => SerdeXml::deserialize(payload.as_bytes()).map_err(DecodeError::from),
        InputFormat::Json => Ok(SerdeJson::deserialize(config, payload)),
        InputFormat::ArrowIpc | InputFormat::Parquet | InputFormat::Avro => {
            unreachable!("text decode of schema format")
        }
    }
}

fn decode_arrow_ipc(payload: &[u8]) -> Result<Vec<RecordBatch>, DecodeError> {
    crate::runtime_plugins::sdk::decode_record_batches(payload.to_vec())
        .map_err(|err| DecodeError::Arrow(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::configuration::Config;
    use crate::serdes::input_format::InputFormat;
    use crate::serdes::parquet::encode_parquet;
    use arrow::array::Int64Array;
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    #[test]
    fn dispatches_json_csv_and_xml() {
        let json_records = match decode_ingest(
            &Config::new(),
            InputFormat::Json,
            br#"{"name":"json"}"#,
        )
        .unwrap()
        {
            DecodedIngest::Rows(rows) => rows,
            DecodedIngest::Arrow(_) => panic!("json must be rows"),
        };
        let csv_records =
            match decode_ingest(&Config::new(), InputFormat::Csv, b"name,age\ncsv,42\n").unwrap() {
                DecodedIngest::Rows(rows) => rows,
                DecodedIngest::Arrow(_) => panic!("csv must be rows"),
            };
        let xml_records = match decode_ingest(
            &Config::new(),
            InputFormat::Xml,
            b"<items><item><name>xml</name></item></items>",
        )
        .unwrap()
        {
            DecodedIngest::Rows(rows) => rows,
            DecodedIngest::Arrow(_) => panic!("xml must be rows"),
        };

        assert_eq!(json_records.len(), 1);
        assert_eq!(json_records[0]["name"], "json");

        assert_eq!(csv_records.len(), 1);
        assert_eq!(csv_records[0]["name"], "csv");

        assert_eq!(xml_records.len(), 1);
        assert_eq!(xml_records[0]["name"], "xml");
    }

    #[test]
    fn parquet_bytes_decode_to_arrow() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vec![9i64]))]).unwrap();
        let bytes = encode_parquet(&[batch]).unwrap();
        match decode_ingest(&Config::new(), InputFormat::Parquet, &bytes).unwrap() {
            DecodedIngest::Arrow(batches) => {
                assert_eq!(batches[0].num_rows(), 1);
            }
            DecodedIngest::Rows(_) => panic!("parquet must be Arrow"),
        }
    }

    #[test]
    fn parquet_decimal_15_2_coerces_to_skippr_38_9() {
        use crate::ingest::schema_arrow::{
            coerce_record_batch_to_skippr, skippr_schema_from_source, SKIPPR_DECIMAL_PRECISION,
            SKIPPR_DECIMAL_SCALE,
        };
        use arrow::array::Decimal128Array;

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
        let DecodedIngest::Arrow(batches) =
            decode_ingest(&Config::new(), InputFormat::Parquet, &bytes).unwrap()
        else {
            panic!("parquet must be Arrow");
        };
        let skippr_schema = skippr_schema_from_source(&batches[0].schema()).unwrap();
        let coerced = coerce_record_batch_to_skippr(&batches[0], skippr_schema).unwrap();
        let col = coerced
            .column(0)
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(col.precision(), SKIPPR_DECIMAL_PRECISION);
        assert_eq!(col.scale(), SKIPPR_DECIMAL_SCALE);
        assert_eq!(col.value(0), 12345i128 * 10i128.pow(7));
    }

    #[test]
    fn text_format_rejects_invalid_utf8() {
        let err = decode_ingest(&Config::new(), InputFormat::Json, &[0xff, 0xfe]).unwrap_err();
        assert!(err.to_string().contains("UTF-8"));
    }
}
