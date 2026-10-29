use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::helpers::configuration::Config;
use crate::serdes::decode::decode_records;
use crate::serdes::input_format::InputFormat;
use crate::serdes::output_format::OutputFormat;

/// Format-agnostic copy WAL frame. `identity` is original NDJSON source bytes,
/// not sink encode intent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CopyWalRecord {
    pub value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<Vec<u8>>,
}

impl CopyWalRecord {
    /// WAL-size estimate without `Value::to_string()` (which allocates a full JSON document).
    pub fn estimated_wal_bytes(&self) -> u64 {
        json_value_bytes(&self.value)
            + self
                .identity
                .as_ref()
                .map(|bytes| bytes.len() as u64)
                .unwrap_or(0)
    }
}

fn json_value_bytes(value: &Value) -> u64 {
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => json_number_bytes(number),
        Value::String(text) => text.len() as u64 + 2,
        Value::Array(items) => {
            2 + items.iter().map(json_value_bytes).sum::<u64>()
                + items.len().saturating_sub(1) as u64
        }
        Value::Object(map) => {
            2 + map
                .iter()
                .map(|(key, child)| key.len() as u64 + 3 + json_value_bytes(child))
                .sum::<u64>()
                + map.len().saturating_sub(1) as u64
        }
    }
}

fn json_number_bytes(number: &serde_json::Number) -> u64 {
    if let Some(signed) = number.as_i64() {
        return signed_digit_bytes(signed);
    }
    if let Some(unsigned) = number.as_u64() {
        return unsigned_digit_bytes(unsigned);
    }
    24
}

fn signed_digit_bytes(value: i64) -> u64 {
    if value == 0 {
        return 1;
    }
    let mut digits = if value < 0 { 1 } else { 0 };
    let mut rest = value.unsigned_abs();
    while rest > 0 {
        digits += 1;
        rest /= 10;
    }
    digits
}

fn unsigned_digit_bytes(value: u64) -> u64 {
    if value == 0 {
        return 1;
    }
    let mut digits = 0u64;
    let mut rest = value;
    while rest > 0 {
        digits += 1;
        rest /= 10;
    }
    digits
}

#[derive(Debug, Error)]
pub enum CopyDecodeError {
    #[error("{0}")]
    Input(String),
}

#[derive(Debug, Error)]
pub enum CopyEncodeError {
    #[error("{0}")]
    Output(String),
}

pub fn decode_copy(
    config: &Config,
    format: InputFormat,
    payload: &str,
) -> Result<Vec<CopyWalRecord>, CopyDecodeError> {
    if matches!(format, InputFormat::ArrowIpc | InputFormat::Parquet) {
        return Err(CopyDecodeError::Input(
            "type: copy does not accept Arrow IPC input".to_string(),
        ));
    }
    if let Some(records) = identity_ndjson(config, format, payload) {
        return Ok(records);
    }
    let values = decode_records(config, format, payload).map_err(|err| {
        CopyDecodeError::Input(format!("unparseable {} payload: {err}", format.as_str()))
    })?;
    Ok(explode_arrays(values)
        .into_iter()
        .map(|value| CopyWalRecord {
            value,
            identity: None,
        })
        .collect())
}

pub fn encode_copy(
    output: OutputFormat,
    records: &[CopyWalRecord],
) -> Result<Vec<u8>, CopyEncodeError> {
    match output {
        OutputFormat::Jsonl => encode_jsonl(records),
        OutputFormat::Csv => encode_csv(records),
        OutputFormat::Xml => encode_xml(records),
        OutputFormat::Parquet | OutputFormat::Native => Err(CopyEncodeError::Output(format!(
            "type: copy cannot encode {output}"
        ))),
    }
}

fn explode_arrays(values: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    for value in values {
        match value {
            Value::Array(items) => out.extend(items),
            other => out.push(other),
        }
    }
    out
}

fn identity_ndjson(
    config: &Config,
    format: InputFormat,
    payload: &str,
) -> Option<Vec<CopyWalRecord>> {
    if format != InputFormat::Json
        || config.get_enable_single_quote_parsing()
        || config.get_enable_unicode_parsing()
    {
        return None;
    }
    let trimmed = payload.trim();
    if trimmed.is_empty() || trimmed.starts_with('[') {
        return None;
    }
    let mut records = Vec::new();
    for line in payload.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        if content.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str::<Value>(content).ok()?;
        let mut identity = line.as_bytes().to_vec();
        if !identity.ends_with(&[b'\n']) {
            identity.push(b'\n');
        }
        records.push(CopyWalRecord {
            value,
            identity: Some(identity),
        });
    }
    if records.is_empty() {
        None
    } else {
        Some(records)
    }
}

fn encode_jsonl(records: &[CopyWalRecord]) -> Result<Vec<u8>, CopyEncodeError> {
    if records.iter().all(|record| record.identity.is_some()) {
        let mut out = Vec::new();
        for record in records {
            out.extend_from_slice(record.identity.as_ref().expect("checked"));
        }
        return Ok(out);
    }
    let mut out = Vec::new();
    for record in records {
        let mut line = serde_json::to_vec(&record.value)
            .map_err(|err| CopyEncodeError::Output(format!("jsonl encode failed: {err}")))?;
        line.push(b'\n');
        out.extend_from_slice(&line);
    }
    Ok(out)
}

fn encode_csv(records: &[CopyWalRecord]) -> Result<Vec<u8>, CopyEncodeError> {
    let mut header: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<String>> = Vec::new();
    for record in records {
        let object = record.value.as_object().ok_or_else(|| {
            CopyEncodeError::Output("CSV encode requires each record to be a JSON object".into())
        })?;
        let keys = csv_keys(object);
        match &header {
            None => header = Some(keys),
            Some(expected) if expected == &keys => {}
            Some(_) => {
                return Err(CopyEncodeError::Output(
                    "CSV encode requires the same columns on every record".into(),
                ));
            }
        }
        let header = header.as_ref().expect("set");
        let mut row = Vec::with_capacity(header.len());
        for key in header {
            row.push(csv_cell(object.get(key).unwrap_or(&Value::Null))?);
        }
        rows.push(row);
    }
    let mut writer = csv::WriterBuilder::new()
        .has_headers(true)
        .from_writer(Vec::new());
    if let Some(header) = header {
        writer
            .write_record(&header)
            .map_err(|err| CopyEncodeError::Output(err.to_string()))?;
        for row in rows {
            writer
                .write_record(&row)
                .map_err(|err| CopyEncodeError::Output(err.to_string()))?;
        }
    }
    writer
        .into_inner()
        .map_err(|err| CopyEncodeError::Output(err.to_string()))
}

fn csv_keys(object: &Map<String, Value>) -> Vec<String> {
    let mut keys: Vec<String> = object.keys().cloned().collect();
    keys.sort();
    keys
}

fn csv_cell(value: &Value) -> Result<String, CopyEncodeError> {
    match value {
        Value::Null => Ok(String::new()),
        Value::Bool(flag) => Ok(flag.to_string()),
        Value::Number(number) => Ok(number.to_string()),
        Value::String(text) => Ok(text.clone()),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(value)
            .map_err(|err| CopyEncodeError::Output(format!("CSV nested encode failed: {err}"))),
    }
}

fn encode_xml(records: &[CopyWalRecord]) -> Result<Vec<u8>, CopyEncodeError> {
    let mut out = String::from("<records>");
    for record in records {
        out.push_str("<record>");
        write_xml_value(&mut out, &record.value)?;
        out.push_str("</record>");
    }
    out.push_str("</records>");
    Ok(out.into_bytes())
}

fn write_xml_value(out: &mut String, value: &Value) -> Result<(), CopyEncodeError> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if !is_xml_name(key) {
                    return Err(CopyEncodeError::Output(format!("illegal XML name '{key}'")));
                }
                match child {
                    Value::Array(items) => {
                        for item in items {
                            write_xml_open(out, key);
                            write_xml_value(out, item)?;
                            write_xml_close(out, key);
                        }
                    }
                    other => {
                        write_xml_open(out, key);
                        write_xml_value(out, other)?;
                        write_xml_close(out, key);
                    }
                }
            }
            Ok(())
        }
        Value::Array(_) => Err(CopyEncodeError::Output(
            "XML encode requires objects at the record root".into(),
        )),
        Value::Null => Ok(()),
        Value::Bool(flag) => {
            out.push_str(&flag.to_string());
            Ok(())
        }
        Value::Number(number) => {
            out.push_str(&number.to_string());
            Ok(())
        }
        Value::String(text) => {
            write_xml_text(out, text);
            Ok(())
        }
    }
}

fn write_xml_open(out: &mut String, key: &str) {
    out.push('<');
    out.push_str(key);
    out.push('>');
}

fn write_xml_close(out: &mut String, key: &str) {
    out.push('<');
    out.push('/');
    out.push_str(key);
    out.push('>');
}

fn write_xml_text(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
}

fn is_xml_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIXTURE: &[(&str, &str)] = &[("id", "1"), ("name", "alpha"), ("region", "eu")];

    fn flat_json() -> String {
        r#"{"id":"1","name":"alpha","region":"eu"}"#.to_string()
    }

    fn flat_csv() -> String {
        "id,name,region\n1,alpha,eu\n".to_string()
    }

    fn flat_xml() -> String {
        "<items><item><id>1</id><name>alpha</name><region>eu</region></item></items>".to_string()
    }

    fn cell_map(records: &[CopyWalRecord]) -> Map<String, Value> {
        records[0].value.as_object().cloned().unwrap()
    }

    #[test]
    fn nine_cell_decode_encode_matrix() {
        let config = Config::new();
        let sources = [
            (InputFormat::Json, flat_json()),
            (InputFormat::Csv, flat_csv()),
            (InputFormat::Xml, flat_xml()),
        ];
        let sinks = [OutputFormat::Jsonl, OutputFormat::Csv, OutputFormat::Xml];
        for (input, payload) in sources {
            for output in sinks {
                let decoded = decode_copy(&config, input, &payload).unwrap();
                assert_eq!(decoded.len(), 1, "{input:?} -> {output:?}");
                let encoded = encode_copy(output, &decoded).unwrap();
                let round_trip_format = match output {
                    OutputFormat::Jsonl => InputFormat::Json,
                    OutputFormat::Csv => InputFormat::Csv,
                    OutputFormat::Xml => InputFormat::Xml,
                    _ => unreachable!(),
                };
                let again = decode_copy(
                    &config,
                    round_trip_format,
                    std::str::from_utf8(&encoded).unwrap(),
                )
                .unwrap();
                let object = cell_map(&again);
                for (key, expect) in FIXTURE {
                    assert_eq!(
                        object.get(*key).and_then(Value::as_str),
                        Some(*expect),
                        "{input:?} -> {output:?} missing {key}"
                    );
                }
            }
        }
    }

    #[test]
    fn copy_rejects_arrow_ipc_and_parquet_text() {
        let config = Config::new();
        for format in [InputFormat::ArrowIpc, InputFormat::Parquet] {
            let err = decode_copy(&config, format, "").unwrap_err();
            assert!(
                err.to_string().contains("does not accept Arrow IPC input"),
                "{format:?}: {err}"
            );
        }
    }

    #[test]
    fn identity_ndjson_keeps_original_bytes() {
        let config = Config::new();
        let payload = "{\"id\":1,\"name\":\"keep\"}\n";
        let decoded = decode_copy(&config, InputFormat::Json, payload).unwrap();
        assert_eq!(decoded[0].identity.as_deref(), Some(payload.as_bytes()));
        let encoded = encode_copy(OutputFormat::Jsonl, &decoded).unwrap();
        assert_eq!(encoded, payload.as_bytes());
    }

    #[test]
    fn json_array_canonicalizes() {
        let config = Config::new();
        let decoded = decode_copy(&config, InputFormat::Json, r#"[{"id":1},{"id":2}]"#).unwrap();
        assert_eq!(decoded.len(), 2);
        assert!(decoded.iter().all(|record| record.identity.is_none()));
        let encoded =
            String::from_utf8(encode_copy(OutputFormat::Jsonl, &decoded).unwrap()).unwrap();
        assert_eq!(encoded, "{\"id\":1}\n{\"id\":2}\n");
    }

    #[test]
    fn pretty_json_canonicalizes() {
        let config = Config::new();
        let payload = "{\n  \"id\": 1\n}\n";
        let decoded = decode_copy(&config, InputFormat::Json, payload).unwrap();
        assert!(decoded[0].identity.is_none());
        let encoded =
            String::from_utf8(encode_copy(OutputFormat::Jsonl, &decoded).unwrap()).unwrap();
        assert_eq!(encoded, "{\"id\":1}\n");
    }

    #[test]
    fn nested_json_to_csv_stringifies_objects_and_arrays() {
        let config = Config::new();
        let decoded = decode_copy(
            &config,
            InputFormat::Json,
            r#"{"id":1,"detail":{"rmc":"x"},"tags":["a","b"]}"#,
        )
        .unwrap();
        let csv = String::from_utf8(encode_copy(OutputFormat::Csv, &decoded).unwrap()).unwrap();
        let mut reader = csv::ReaderBuilder::new().from_reader(csv.as_bytes());
        let headers = reader.headers().unwrap().clone();
        let row = reader.records().next().unwrap().unwrap();
        let map: std::collections::BTreeMap<String, String> = headers
            .iter()
            .zip(row.iter())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(map.get("id").map(String::as_str), Some("1"));
        assert_eq!(
            serde_json::from_str::<Value>(map.get("detail").unwrap()).unwrap(),
            json!({"rmc": "x"})
        );
        assert_eq!(
            serde_json::from_str::<Value>(map.get("tags").unwrap()).unwrap(),
            json!(["a", "b"])
        );
    }

    #[test]
    fn nested_xml_to_csv_stringifies_objects() {
        let config = Config::new();
        let decoded = decode_copy(
            &config,
            InputFormat::Xml,
            "<records><record><id>1</id><detail><rmc>x</rmc></detail></record></records>",
        )
        .unwrap();
        let csv = String::from_utf8(encode_copy(OutputFormat::Csv, &decoded).unwrap()).unwrap();
        let mut reader = csv::ReaderBuilder::new().from_reader(csv.as_bytes());
        let headers = reader.headers().unwrap().clone();
        let row = reader.records().next().unwrap().unwrap();
        let map: std::collections::BTreeMap<String, String> = headers
            .iter()
            .zip(row.iter())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(map.get("id").map(String::as_str), Some("1"));
        assert_eq!(
            serde_json::from_str::<Value>(map.get("detail").unwrap()).unwrap(),
            json!({"rmc": "x"})
        );
    }

    #[test]
    fn illegal_xml_name_fails_closed() {
        let record = CopyWalRecord {
            value: json!({ "1bad": "x" }),
            identity: None,
        };
        let err = encode_copy(OutputFormat::Xml, &[record]).unwrap_err();
        assert!(err.to_string().contains("illegal XML name"));
    }

    #[test]
    fn parquet_encode_fails_closed() {
        let record = CopyWalRecord {
            value: json!({"id": 1}),
            identity: None,
        };
        assert!(encode_copy(OutputFormat::Parquet, &[record]).is_err());
    }

    #[test]
    fn estimated_wal_bytes_counts_identity_without_display() {
        let identity = b"{\"id\":1,\"name\":\"alpha\"}\n".to_vec();
        let record = CopyWalRecord {
            value: json!({"id": 1, "name": "alpha"}),
            identity: Some(identity.clone()),
        };
        let bytes = record.estimated_wal_bytes();
        assert!(bytes >= identity.len() as u64);
        assert!(
            bytes < 200,
            "estimate should stay near the JSON size, got {bytes}"
        );
    }

    #[test]
    fn identity_jsonl_roundtrip_stays_linear() {
        let config = Config::new();
        let n = 8_000;
        let mut payload = String::new();
        for i in 0..n {
            payload.push_str(&format!("{{\"id\":{i},\"name\":\"alpha\"}}\n"));
        }
        let started = std::time::Instant::now();
        let decoded = decode_copy(&config, InputFormat::Json, &payload).unwrap();
        assert_eq!(decoded.len(), n);
        let encoded = encode_copy(OutputFormat::Jsonl, &decoded).unwrap();
        let estimate: u64 = decoded.iter().map(CopyWalRecord::estimated_wal_bytes).sum();
        let elapsed = started.elapsed();
        assert_eq!(encoded, payload.as_bytes());
        assert!(estimate >= payload.len() as u64);
        assert!(
            elapsed.as_millis() < 1_500,
            "copy identity jsonl of {n} rows took {elapsed:?}"
        );
    }

    #[test]
    #[ignore = "benchmark; run with `cargo test copy -- --ignored --nocapture`"]
    fn bench_copy_identity_jsonl_10k() {
        let config = Config::new();
        let n = 10_000;
        let mut payload = String::new();
        for i in 0..n {
            payload.push_str(&format!(
                "{{\"id\":{i},\"name\":\"alpha\",\"region\":\"eu\",\"value\":{}}}\n",
                i * 3
            ));
        }
        let started = std::time::Instant::now();
        let decoded = decode_copy(&config, InputFormat::Json, &payload).unwrap();
        let encoded = encode_copy(OutputFormat::Jsonl, &decoded).unwrap();
        let estimate: u64 = decoded.iter().map(CopyWalRecord::estimated_wal_bytes).sum();
        let elapsed = started.elapsed();
        assert_eq!(decoded.len(), n);
        assert_eq!(encoded, payload.as_bytes());
        println!(
            "copy identity jsonl rows={n} input_bytes={} duration_ms={:.2} records_per_sec={:.0} estimated_wal_bytes={estimate}",
            payload.len(),
            elapsed.as_secs_f64() * 1000.0,
            n as f64 / elapsed.as_secs_f64().max(1e-9),
        );
    }
}
