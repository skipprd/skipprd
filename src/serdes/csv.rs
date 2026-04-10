extern crate csv;

use csv::StringRecord;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CsvDecodeError {
    #[error("CSV record parse failed: {0}")]
    Record(#[from] csv::Error),
}

pub struct SerderCsv;

impl SerderCsv {
    pub fn deserialize(record: &str) -> Result<Vec<Value>, CsvDecodeError> {
        let normalized = record.replace("\r\n", "\n");
        let delimiter = Self::detect_delimiter(&normalized);
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(false)
            .flexible(true)
            .from_reader(normalized.as_bytes());

        let mut records = reader.records();
        let Some(first_record) = records.next() else {
            return Ok(Vec::new());
        };
        let first_record = first_record?;
        let headers = Self::headers_for(&first_record);
        let mut messages = Vec::new();

        if headers.is_none() {
            messages.push(Self::record_to_object(None, &first_record));
        }

        for record in records {
            let record = record?;
            messages.push(Self::record_to_object(headers.as_deref(), &record));
        }

        Ok(messages)
    }

    fn record_to_object(headers: Option<&[String]>, record: &StringRecord) -> Value {
        let mut obj = serde_json::Map::new();
        match headers {
            Some(headers) => {
                for (idx, value) in record.iter().enumerate() {
                    let key = headers.get(idx).cloned().unwrap_or_else(|| idx.to_string());
                    obj.insert(key, Value::String(value.to_string()));
                }
            }
            None => {
                for (idx, value) in record.iter().enumerate() {
                    obj.insert(idx.to_string(), Value::String(value.to_string()));
                }
            }
        }
        Value::Object(obj)
    }

    fn headers_for(first_record: &StringRecord) -> Option<Vec<String>> {
        let is_header_row = !first_record.is_empty()
            && first_record
                .iter()
                .all(|item| !item.trim().is_empty() && item.parse::<i64>().is_err());

        is_header_row.then(|| first_record.iter().map(|item| item.to_string()).collect())
    }

    fn detect_delimiter(record: &str) -> u8 {
        let first_line = record
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        let delimiters = [b',', b';', b'\t', b'|'];
        let mut chosen_delimiter = b',';
        let mut max_fields = 0usize;

        for delimiter in delimiters {
            let fields = first_line.split(delimiter as char).count();
            if fields > max_fields {
                max_fields = fields;
                chosen_delimiter = delimiter;
            }
        }

        chosen_delimiter
    }
}

#[cfg(test)]
mod tests_csv {
    use super::SerderCsv;
    use serde_json::json;

    #[test]
    fn test_comma_delimiter() {
        let input = "name,age\nJohn,30\nDoe,25\n";
        let output = SerderCsv::deserialize(input).unwrap();

        let expected = vec![
            json!({"name": "John", "age": "30"}),
            json!({"name": "Doe", "age": "25"}),
        ];
        assert_eq!(output, expected);
    }

    #[test]
    fn test_sequential_documents_are_isolated() {
        let comma_csv = "name,age\nJohn,30\n";
        let semicolon_csv = "city;country\nParis;France\n";

        let comma_output = SerderCsv::deserialize(comma_csv).unwrap();
        let semicolon_output = SerderCsv::deserialize(semicolon_csv).unwrap();

        assert_eq!(comma_output, vec![json!({"name": "John", "age": "30"})]);
        assert_eq!(
            semicolon_output,
            vec![json!({"city": "Paris", "country": "France"})]
        );
    }

    #[test]
    fn test_headerless_document_uses_positional_keys() {
        let input = "Alice,23\nBob,28";
        let output = SerderCsv::deserialize(input).unwrap();

        let expected = vec![
            json!({"0": "Alice", "1": "23"}),
            json!({"0": "Bob", "1": "28"}),
        ];
        assert_eq!(output, expected);
    }

    #[test]
    fn test_mixed_shape_documents_decode_independently() {
        let headerful = "name,age\nJane,20";
        let headerless = "Alice,23\nBob,28";

        let headerful_output = SerderCsv::deserialize(headerful).unwrap();
        let headerless_output = SerderCsv::deserialize(headerless).unwrap();

        assert_eq!(headerful_output, vec![json!({"name": "Jane", "age": "20"})]);
        assert_eq!(
            headerless_output,
            vec![
                json!({"0": "Alice", "1": "23"}),
                json!({"0": "Bob", "1": "28"}),
            ]
        );
    }
}
