#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InputFormat {
    ArrowIpc,
    Csv,
    Xml,
    #[default]
    Json,
}

impl InputFormat {
    pub fn from_option(value: Option<&str>) -> Self {
        value.map(Self::from).unwrap_or_default()
    }

    pub fn requires_whole_payload(self) -> bool {
        matches!(self, Self::ArrowIpc | Self::Csv | Self::Xml)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ArrowIpc => "arrow_ipc",
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
    }

    #[test]
    fn csv_and_xml_require_whole_payloads() {
        assert!(InputFormat::ArrowIpc.requires_whole_payload());
        assert!(InputFormat::Csv.requires_whole_payload());
        assert!(InputFormat::Xml.requires_whole_payload());
        assert!(!InputFormat::Json.requires_whole_payload());
    }
}
