use std::sync::Once;

use serde_derive::{Deserialize, Serialize};

/// WAL partition fingerprint for copy frames. Never empty (empty recomputes an
/// Arrow schema hash) and never an output format.
pub const COPY_WAL_FINGERPRINT: &str = "copy";

/// How a pipeline moves records. YAML `type:` is this enum; `cdc:` is settings.
///
/// Config serde stays a lowercase string so connect-gen and `type: copy` keep
/// working. The runtime plugin bincode field uses [`wire`] (`u32` 0/1/2) so a
/// layout mismatch cannot treat a string length as a `HostFrame` tag.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase", try_from = "String", into = "String")]
#[repr(u32)]
pub enum PipelineType {
    #[default]
    Ingest = 0,
    Copy = 1,
    Cdc = 2,
}

impl PipelineType {
    pub const ALL: [PipelineType; 3] =
        [PipelineType::Ingest, PipelineType::Copy, PipelineType::Cdc];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ingest => "ingest",
            Self::Copy => "copy",
            Self::Cdc => "cdc",
        }
    }

    /// Deadletter WAL is Arrow (`commit_deadletters`). Format allow-list is
    /// ingest (parquet/jsonl), never copy Identity-PUT csv/xml.
    pub const fn deadletter_format_mode() -> Self {
        Self::Ingest
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ingest" => Some(Self::Ingest),
            "copy" => Some(Self::Copy),
            "cdc" => Some(Self::Cdc),
            _ => None,
        }
    }
}

impl TryFrom<String> for PipelineType {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, String> {
        Self::parse(&raw).ok_or_else(|| {
            format!(
                "unknown pipeline type '{raw}'; expected one of {:?}",
                Self::ALL.map(Self::as_str)
            )
        })
    }
}

impl From<PipelineType> for String {
    fn from(value: PipelineType) -> Self {
        value.as_str().to_string()
    }
}

/// Bincode encoding for `RuntimeExecutionContext.pipeline_type`.
pub mod wire {
    use super::PipelineType;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        value: &PipelineType,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(value.as_str())
        } else {
            serializer.serialize_u32(*value as u32)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<PipelineType, D::Error> {
        if deserializer.is_human_readable() {
            let raw = String::deserialize(deserializer)?;
            PipelineType::try_from(raw).map_err(serde::de::Error::custom)
        } else {
            match u32::deserialize(deserializer)? {
                0 => Ok(PipelineType::Ingest),
                1 => Ok(PipelineType::Copy),
                2 => Ok(PipelineType::Cdc),
                other => Err(serde::de::Error::invalid_value(
                    serde::de::Unexpected::Unsigned(u64::from(other)),
                    &"pipeline type tag 0=ingest, 1=copy, 2=cdc",
                )),
            }
        }
    }
}

impl std::fmt::Display for PipelineType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Resolve YAML `type:` against the presence of a `cdc:` block.
///
/// Only omitted `type` infers CDC from `cdc:`. That inference logs once.
pub fn resolve_pipeline_type(
    declared: Option<PipelineType>,
    has_cdc_block: bool,
) -> Result<PipelineType, String> {
    match (declared, has_cdc_block) {
        (None, false) => Ok(PipelineType::Ingest),
        (None, true) => {
            warn_cdc_inference_once();
            Ok(PipelineType::Cdc)
        }
        (Some(PipelineType::Ingest), true) => Err(
            "type: ingest cannot include a cdc: block; set type: cdc or remove cdc:".to_string(),
        ),
        (Some(PipelineType::Copy), true) => {
            Err("type: copy cannot include a cdc: block".to_string())
        }
        (Some(kind), _) => Ok(kind),
    }
}

fn warn_cdc_inference_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::warn!(
            "pipelines.*.cdc without type: cdc is deprecated; inferred type: cdc. Set type: cdc explicitly."
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadletter_format_mode_is_ingest() {
        assert_eq!(PipelineType::deadletter_format_mode(), PipelineType::Ingest);
    }

    #[test]
    fn parse_accepts_legacy_uppercase_ingest() {
        assert_eq!(PipelineType::parse("INGEST"), Some(PipelineType::Ingest));
        assert_eq!(PipelineType::parse("Copy"), Some(PipelineType::Copy));
        assert_eq!(PipelineType::parse("cdc"), Some(PipelineType::Cdc));
        assert!(PipelineType::parse("passthrough").is_none());
        assert!(PipelineType::parse("hive").is_none());
    }

    #[test]
    fn omitted_type_without_cdc_is_ingest() {
        assert_eq!(
            resolve_pipeline_type(None, false).unwrap(),
            PipelineType::Ingest
        );
    }

    #[test]
    fn omitted_type_with_cdc_infers_cdc() {
        assert_eq!(
            resolve_pipeline_type(None, true).unwrap(),
            PipelineType::Cdc
        );
    }

    #[test]
    fn ingest_plus_cdc_fails_closed() {
        let err = resolve_pipeline_type(Some(PipelineType::Ingest), true).unwrap_err();
        assert!(err.contains("type: ingest cannot include a cdc: block"));
    }

    #[test]
    fn copy_plus_cdc_fails_closed() {
        let err = resolve_pipeline_type(Some(PipelineType::Copy), true).unwrap_err();
        assert!(err.contains("type: copy cannot include a cdc: block"));
    }

    #[test]
    fn explicit_cdc_without_block_is_cdc() {
        assert_eq!(
            resolve_pipeline_type(Some(PipelineType::Cdc), false).unwrap(),
            PipelineType::Cdc
        );
    }

    #[test]
    fn explicit_copy_without_cdc_is_copy() {
        assert_eq!(
            resolve_pipeline_type(Some(PipelineType::Copy), false).unwrap(),
            PipelineType::Copy
        );
    }

    #[test]
    fn bincode_roundtrips_copy_as_copy() {
        let bytes = bincode::serialize(&PipelineType::Copy).unwrap();
        let decoded: PipelineType = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded, PipelineType::Copy);
        let as_str: String = bincode::deserialize(&bytes).unwrap();
        assert_eq!(as_str, "copy");
    }

    #[test]
    fn yaml_keeps_lowercase_copy_name() {
        let yaml = serde_yaml::to_string(&PipelineType::Copy).unwrap();
        assert!(yaml.contains("copy"), "{yaml}");
        let decoded: PipelineType = serde_yaml::from_str("copy\n").unwrap();
        assert_eq!(decoded, PipelineType::Copy);
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct WireCopy {
        #[serde(with = "super::wire")]
        kind: PipelineType,
    }

    #[test]
    fn protocol_wire_encodes_copy_as_u32() {
        let bytes = bincode::serialize(&WireCopy {
            kind: PipelineType::Copy,
        })
        .unwrap();
        let decoded: WireCopy = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.kind, PipelineType::Copy);
        assert_eq!(bytes, 1u32.to_le_bytes());
    }

    #[test]
    fn protocol_21_bincode_string_is_rejected_on_the_wire() {
        let proto21 = bincode::serialize(&"copy".to_string()).unwrap();
        let err = bincode::deserialize::<WireCopy>(&proto21).unwrap_err();
        assert!(err.to_string().contains("pipeline type tag"), "{err}");
    }
}
