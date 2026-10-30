#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InputFormat {
    ArrowIpc,
    Parquet,
    Avro,
    Csv,
    Xml,
    #[default]
    Json,
}

impl InputFormat {
    pub fn from_option(value: Option<&str>) -> Self {
        value.map(Self::from).unwrap_or_default()
    }

    /// Omitted `format` on a source plugin. Otlp is Arrow IPC; everything else is JSON.
    pub fn default_for_source_plugin(plugin_name: &str) -> Self {
        match plugin_name {
            "Otlp" => Self::ArrowIpc,
            _ => Self::Json,
        }
    }

    pub fn requires_whole_payload(self) -> bool {
        matches!(
            self,
            Self::ArrowIpc | Self::Parquet | Self::Avro | Self::Csv | Self::Xml
        )
    }

    pub fn is_text(self) -> bool {
        matches!(self, Self::Json | Self::Csv | Self::Xml)
    }

    pub fn is_schema_arrow(self) -> bool {
        matches!(self, Self::ArrowIpc | Self::Parquet | Self::Avro)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ArrowIpc => "arrow_ipc",
            Self::Parquet => "parquet",
            Self::Avro => "avro",
            Self::Csv => "csv",
            Self::Xml => "xml",
            Self::Json => "json",
        }
    }
}

impl From<&str> for InputFormat {
    fn from(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "arrow" | "arrow_ipc" | "arrow-ipc" | "ipc" => Self::ArrowIpc,
            "parquet" => Self::Parquet,
            "avro" => Self::Avro,
            "csv" => Self::Csv,
            "xml" => Self::Xml,
            _ => Self::Json,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::InputFormat;

    #[test]
    fn unknown_formats_default_to_json() {
        assert_eq!(InputFormat::from("row"), InputFormat::Json);
        assert_eq!(InputFormat::from(""), InputFormat::Json);
        assert_eq!(InputFormat::from("parquet"), InputFormat::Parquet);
        assert_ne!(InputFormat::from("parquet"), InputFormat::Json);
        assert_eq!(InputFormat::from("avro"), InputFormat::Avro);
        assert_ne!(InputFormat::from("avro"), InputFormat::Json);
    }

    #[test]
    fn csv_and_xml_require_whole_payloads() {
        assert!(InputFormat::ArrowIpc.requires_whole_payload());
        assert!(InputFormat::Parquet.requires_whole_payload());
        assert!(InputFormat::Avro.requires_whole_payload());
        assert!(InputFormat::Csv.requires_whole_payload());
        assert!(InputFormat::Xml.requires_whole_payload());
        assert!(!InputFormat::Json.requires_whole_payload());
        assert!(InputFormat::Parquet.is_schema_arrow());
        assert!(InputFormat::Avro.is_schema_arrow());
        assert!(InputFormat::Json.is_text());
        assert!(!InputFormat::Parquet.is_text());
    }

    #[test]
    fn omitted_otlp_format_is_arrow_ipc() {
        assert_eq!(
            InputFormat::default_for_source_plugin("Otlp"),
            InputFormat::ArrowIpc
        );
        assert_eq!(
            InputFormat::default_for_source_plugin("File"),
            InputFormat::Json
        );
    }
}
