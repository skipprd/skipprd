use std::fmt;
use std::io;
use std::str::FromStr;

use serde_derive::{Deserialize, Serialize};

use crate::pipeline_type::PipelineType;
use crate::serdes::input_format::InputFormat;

/// How a sink encodes output.
///
/// File sinks (File, S3, Athena, …) accept [`OutputFormat::Parquet`] and/or
/// [`OutputFormat::Jsonl`]. Copy adds [`OutputFormat::Csv`] and
/// [`OutputFormat::Xml`]. Table sinks (Postgres, Snowflake, Iceberg lakes)
/// declare [`OutputFormat::Native`]: they insert rows into tables and have no
/// file-format setting.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    /// Inserts rows into tables (SQL or Iceberg). Not a file format; users
    /// must omit `format` on these sinks.
    Native,
    Parquet,
    Jsonl,
    Csv,
    Xml,
}

impl OutputFormat {
    /// Error when a file sink is asked to use table encoding.
    pub const FILE_SINK_NEEDS_PARQUET_OR_JSONL: &'static str =
        "this sink writes files and cannot use table encoding. Set format to parquet or jsonl (ingest), or jsonl, csv, or xml (type: copy).";

    /// Serde default when capability formats are skipped on the wire.
    pub fn native_formats() -> &'static [Self] {
        &[Self::Native]
    }

    pub fn is_native(self) -> bool {
        matches!(self, Self::Native)
    }

    pub fn is_native_set(supported: &[Self]) -> bool {
        !supported.is_empty() && supported.iter().all(|format| format.is_native())
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Parquet => "parquet",
            Self::Jsonl => "jsonl",
            Self::Csv => "csv",
            Self::Xml => "xml",
            Self::Native => "",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Parquet => "application/vnd.apache.parquet",
            Self::Jsonl => "application/x-ndjson",
            Self::Csv => "text/csv",
            Self::Xml => "application/xml",
            Self::Native => "application/octet-stream",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Parquet => "parquet",
            Self::Jsonl => "jsonl",
            Self::Csv => "csv",
            Self::Xml => "xml",
        }
    }

    pub fn legal_for(self, pipeline: PipelineType) -> bool {
        match pipeline {
            PipelineType::Copy => matches!(self, Self::Jsonl | Self::Csv | Self::Xml),
            PipelineType::Ingest | PipelineType::Cdc => matches!(self, Self::Parquet | Self::Jsonl),
        }
    }

    /// Object sinks that Identity-PUT copy encodings declare both csv and xml.
    pub fn supports_copy_identity(supported: &[Self]) -> bool {
        supported.contains(&Self::Csv) && supported.contains(&Self::Xml)
    }

    fn default_for_copy_source(source: InputFormat) -> Self {
        match source {
            InputFormat::Json
            | InputFormat::ArrowIpc
            | InputFormat::Parquet
            | InputFormat::Avro => Self::Jsonl,
            InputFormat::Csv => Self::Csv,
            InputFormat::Xml => Self::Xml,
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OutputFormat {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_output_format(value)
    }
}

pub fn parse_output_format(value: &str) -> Result<OutputFormat, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "parquet" => Ok(OutputFormat::Parquet),
        "jsonl" | "json" => Ok(OutputFormat::Jsonl),
        "csv" => Ok(OutputFormat::Csv),
        "xml" => Ok(OutputFormat::Xml),
        "native" | "n/a" | "na" | "null" => Err(format!(
            "'{value}' is not a file format. Use parquet, jsonl, csv, or xml for file sinks, or omit format on table sinks (Postgres, Snowflake, Iceberg lakes)."
        )),
        other => Err(format!(
            "unknown output format '{other}'. File sinks accept parquet, jsonl, csv, or xml."
        )),
    }
}

/// Resolve a configured `format:` against a sink's declared set.
///
/// Table sinks ([`OutputFormat::Native`] only) insert rows and have no file
/// format: a configured `format:` is an error; omitted format is `Ok(None)`.
pub fn resolve_output_format(
    plugin_name: &str,
    supported: &[OutputFormat],
    default_format: Option<OutputFormat>,
    configured: Option<&str>,
    pipeline: PipelineType,
    source: Option<InputFormat>,
) -> Result<Option<OutputFormat>, String> {
    if OutputFormat::is_native_set(supported) {
        if pipeline == PipelineType::Copy {
            return Err(format!(
                "type: copy cannot write to table sink '{plugin_name}'"
            ));
        }
        if let Some(configured) = configured.map(str::trim).filter(|value| !value.is_empty()) {
            return Err(table_sink_rejects_format(plugin_name, configured));
        }
        return Ok(None);
    }
    if supported.iter().any(|format| format.is_native()) {
        return Err(format!(
            "data sink '{plugin_name}' listed both table encoding (writes rows, no files) and file encodings. Use only Native for table sinks, or only file encodings for file sinks."
        ));
    }
    if supported.is_empty() {
        return Err(format!(
            "data sink '{plugin_name}' declares no supported_formats"
        ));
    }
    if pipeline == PipelineType::Copy && !OutputFormat::supports_copy_identity(supported) {
        return Err(format!(
            "type: copy cannot write to '{plugin_name}'; use S3, GCS, AzureBlob, File, or SFTP"
        ));
    }
    let configured = configured.map(str::trim).filter(|value| !value.is_empty());
    let resolved = match configured {
        None if pipeline == PipelineType::Copy => {
            let source = source.ok_or_else(|| {
                format!(
                    "type: copy requires an explicit sink format on '{plugin_name}' when the source format is unknown"
                )
            })?;
            let family = OutputFormat::default_for_copy_source(source);
            if !supported.contains(&family) {
                return Err(format!(
                    "type: copy default format '{family}' is not supported by '{plugin_name}'. Set format: to one of {}",
                    supported.iter().map(OutputFormat::as_str).collect::<Vec<_>>().join(", ")
                ));
            }
            family
        }
        None => default_format.ok_or_else(|| {
            format!("data sink '{plugin_name}' declares formats but has no default_format")
        })?,
        Some(raw) => parse_output_format(raw)?,
    };
    if !resolved.legal_for(pipeline) {
        return Err(format!(
            "pipeline type {pipeline} does not allow format '{resolved}' on '{plugin_name}'"
        ));
    }
    if resolved.is_native() || !supported.contains(&resolved) {
        return Err(format!(
            "data sink '{plugin_name}' does not support format '{resolved}'. Supported: {}",
            supported
                .iter()
                .map(OutputFormat::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(Some(resolved))
}

fn table_sink_rejects_format(plugin_name: &str, configured: &str) -> String {
    format!(
        "data sink '{plugin_name}' writes rows into tables, not Parquet or JSON Lines files, so `format` does not apply. Remove `format: {configured}` from the sink config."
    )
}

pub fn encode_jsonl_batch(batch: &arrow::record_batch::RecordBatch) -> io::Result<Vec<u8>> {
    skippr_object_writer::encode_jsonl_batch(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline_type::PipelineType;
    use arrow::array::{Int32Array, StringArray};

    fn ingest_resolve(
        plugin: &str,
        supported: &[OutputFormat],
        default_format: Option<OutputFormat>,
        configured: Option<&str>,
    ) -> Result<Option<OutputFormat>, String> {
        resolve_output_format(
            plugin,
            supported,
            default_format,
            configured,
            PipelineType::Ingest,
            None,
        )
    }
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use std::sync::Arc;

    #[test]
    fn parse_accepts_parquet_jsonl_and_json_alias() {
        assert_eq!(
            parse_output_format("parquet").unwrap(),
            OutputFormat::Parquet
        );
        assert_eq!(parse_output_format("JSONL").unwrap(), OutputFormat::Jsonl);
        assert_eq!(parse_output_format("json").unwrap(), OutputFormat::Jsonl);
    }

    #[test]
    fn parse_rejects_avro_and_native() {
        let err = parse_output_format("avro").unwrap_err();
        assert!(err.contains("unknown output format"));
        let err = parse_output_format("native").unwrap_err();
        assert!(err.contains("not a file format"));
        assert!(err.contains("omit format on table sinks"));
    }

    #[test]
    fn omitted_format_on_object_sink_is_parquet() {
        let resolved = ingest_resolve(
            "File",
            &[OutputFormat::Parquet, OutputFormat::Jsonl],
            Some(OutputFormat::Parquet),
            None,
        )
        .unwrap();
        assert_eq!(resolved, Some(OutputFormat::Parquet));
    }

    #[test]
    fn file_rejects_avro() {
        let err = ingest_resolve(
            "File",
            &[OutputFormat::Parquet, OutputFormat::Jsonl],
            Some(OutputFormat::Parquet),
            Some("avro"),
        )
        .unwrap_err();
        assert!(err.contains("unknown output format"));
    }

    #[test]
    fn stdout_rejects_parquet() {
        let err = ingest_resolve(
            "Stdout",
            &[OutputFormat::Jsonl],
            Some(OutputFormat::Jsonl),
            Some("parquet"),
        )
        .unwrap_err();
        assert!(err.contains("does not support format 'parquet'"));
    }

    #[test]
    fn warehouse_rejects_configured_format() {
        let err =
            ingest_resolve("Postgres", &[OutputFormat::Native], None, Some("jsonl")).unwrap_err();
        assert!(err.contains("writes rows into tables"));
        assert!(err.contains("Remove `format: jsonl`"));
    }

    #[test]
    fn warehouse_omitted_format_is_none() {
        assert_eq!(
            ingest_resolve("Postgres", &[OutputFormat::Native], None, None).unwrap(),
            None
        );
    }

    #[test]
    fn athena_defaults_parquet_and_rejects_jsonl() {
        assert_eq!(
            ingest_resolve(
                "Athena",
                &[OutputFormat::Parquet],
                Some(OutputFormat::Parquet),
                None
            )
            .unwrap(),
            Some(OutputFormat::Parquet)
        );
        let err = ingest_resolve(
            "Athena",
            &[OutputFormat::Parquet],
            Some(OutputFormat::Parquet),
            Some("jsonl"),
        )
        .unwrap_err();
        assert!(err.contains("does not support format 'jsonl'"));
    }

    #[test]
    fn copy_omitted_format_follows_json_source_not_parquet() {
        let resolved = resolve_output_format(
            "File",
            &[
                OutputFormat::Parquet,
                OutputFormat::Jsonl,
                OutputFormat::Csv,
                OutputFormat::Xml,
            ],
            Some(OutputFormat::Parquet),
            None,
            PipelineType::Copy,
            Some(InputFormat::Json),
        )
        .unwrap();
        assert_eq!(resolved, Some(OutputFormat::Jsonl));
    }

    #[test]
    fn copy_omitted_format_parquet_source_defaults_to_jsonl() {
        let resolved = resolve_output_format(
            "File",
            &[
                OutputFormat::Parquet,
                OutputFormat::Jsonl,
                OutputFormat::Csv,
                OutputFormat::Xml,
            ],
            Some(OutputFormat::Parquet),
            None,
            PipelineType::Copy,
            Some(InputFormat::Parquet),
        )
        .unwrap();
        assert_eq!(resolved, Some(OutputFormat::Jsonl));
    }

    #[test]
    fn copy_rejects_parquet() {
        let err = resolve_output_format(
            "File",
            &[
                OutputFormat::Parquet,
                OutputFormat::Jsonl,
                OutputFormat::Csv,
                OutputFormat::Xml,
            ],
            Some(OutputFormat::Parquet),
            Some("parquet"),
            PipelineType::Copy,
            Some(InputFormat::Json),
        )
        .unwrap_err();
        assert!(err.contains("does not allow format 'parquet'"));
    }

    #[test]
    fn ingest_rejects_csv() {
        let err = ingest_resolve(
            "File",
            &[
                OutputFormat::Parquet,
                OutputFormat::Jsonl,
                OutputFormat::Csv,
                OutputFormat::Xml,
            ],
            Some(OutputFormat::Parquet),
            Some("csv"),
        )
        .unwrap_err();
        assert!(err.contains("does not allow format 'csv'"));
    }

    #[test]
    fn copy_rejects_amqp() {
        let err = resolve_output_format(
            "Amqp",
            &[OutputFormat::Jsonl],
            Some(OutputFormat::Jsonl),
            None,
            PipelineType::Copy,
            Some(InputFormat::Json),
        )
        .unwrap_err();
        assert!(err.contains("cannot write to 'Amqp'"));
    }

    #[test]
    fn copy_rejects_stdout() {
        let err = resolve_output_format(
            "Stdout",
            &[OutputFormat::Jsonl],
            Some(OutputFormat::Jsonl),
            Some("jsonl"),
            PipelineType::Copy,
            Some(InputFormat::Json),
        )
        .unwrap_err();
        assert!(err.contains("cannot write to 'Stdout'"));
    }

    #[test]
    fn copy_rejects_native_table_sink() {
        let err = resolve_output_format(
            "Postgres",
            &[OutputFormat::Native],
            None,
            None,
            PipelineType::Copy,
            Some(InputFormat::Json),
        )
        .unwrap_err();
        assert!(err.contains("cannot write to table sink 'Postgres'"));
    }

    #[test]
    fn amqp_omitted_format_is_jsonl() {
        assert_eq!(
            ingest_resolve(
                "Amqp",
                &[OutputFormat::Jsonl],
                Some(OutputFormat::Jsonl),
                None
            )
            .unwrap(),
            Some(OutputFormat::Jsonl)
        );
    }

    #[test]
    fn jsonl_encoder_writes_one_object_per_line() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int32Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec!["a", "b"])),
            ],
        )
        .unwrap();
        let encoded = String::from_utf8(encode_jsonl_batch(&batch).unwrap()).unwrap();
        let lines: Vec<&str> = encoded.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["id"], 1);
        assert_eq!(first["name"], "a");
    }
}
