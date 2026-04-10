use serde_json::Value;
use thiserror::Error;

use crate::serdes::csv::{CsvDecodeError, SerderCsv};
use crate::serdes::input_format::InputFormat;
use crate::serdes::json::SerdeJson;
use crate::serdes::xml::{SerdeXml, XmlDecodeError};

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error(transparent)]
    Csv(#[from] CsvDecodeError),
    #[error(transparent)]
    Xml(#[from] XmlDecodeError),
}

pub fn decode_records(format: InputFormat, payload: &str) -> Result<Vec<Value>, DecodeError> {
    match format {
        InputFormat::Csv => SerderCsv::deserialize(payload).map_err(DecodeError::from),
        InputFormat::Xml => SerdeXml::deserialize(payload.as_bytes()).map_err(DecodeError::from),
        InputFormat::Json => Ok(SerdeJson::deserialize(payload)),
    }
}

#[cfg(test)]
mod tests {
    use crate::serdes::decode::decode_records;
    use crate::serdes::input_format::InputFormat;

    #[test]
    fn dispatches_json_csv_and_xml() {
        let json_records = decode_records(InputFormat::Json, r#"{"name":"json"}"#).unwrap();
        let csv_records = decode_records(InputFormat::Csv, "name,age\ncsv,42\n").unwrap();
        let xml_records = decode_records(
            InputFormat::Xml,
            "<items><item><name>xml</name></item></items>",
        )
        .unwrap();

        assert_eq!(json_records.len(), 1);
        assert_eq!(json_records[0]["name"], "json");

        assert_eq!(csv_records.len(), 1);
        assert_eq!(csv_records[0]["name"], "csv");

        assert_eq!(xml_records.len(), 1);
        assert_eq!(xml_records[0]["name"], "xml");
    }
}
