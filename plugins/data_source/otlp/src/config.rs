use std::collections::BTreeSet;

use serde_derive::Deserialize;
use skippr_runtime_sdk::helpers::plugin_config::PluginConfigEntry;

/// Maximum accepted OTLP request body (HTTP Content-Length / gRPC message).
pub const OTLP_MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OtlpSignal {
    Traces,
    Logs,
    Metrics,
}

impl OtlpSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Traces => "traces",
            Self::Logs => "logs",
            Self::Metrics => "metrics",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OtlpConfigRaw {
    #[serde(default)]
    listen_address_grpc: Option<String>,
    #[serde(default)]
    listen_address_http: Option<String>,
    #[serde(default)]
    signals: Option<Vec<OtlpSignal>>,
    #[serde(default)]
    auth_token: Option<String>,
    /// `None` keeps all attributes. `Some([])` drops every attribute.
    #[serde(default)]
    attribute_allowlist: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtlpConfig {
    pub listen_address_grpc: String,
    pub listen_address_http: String,
    pub signals: BTreeSet<OtlpSignal>,
    pub auth_token: Option<String>,
    pub attribute_allowlist: Option<Vec<String>>,
}

impl OtlpConfig {
    pub fn try_from_raw(raw: OtlpConfigRaw) -> Result<Self, String> {
        let signals = match raw.signals {
            None => BTreeSet::from([OtlpSignal::Traces, OtlpSignal::Logs, OtlpSignal::Metrics]),
            Some(list) if list.is_empty() => {
                return Err("OtlpConfig.signals must not be empty".to_string());
            }
            Some(list) => list.into_iter().collect(),
        };
        Ok(Self {
            listen_address_grpc: raw
                .listen_address_grpc
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "0.0.0.0:4317".to_string()),
            listen_address_http: raw
                .listen_address_http
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "0.0.0.0:4318".to_string()),
            signals,
            auth_token: raw.auth_token.filter(|s| !s.is_empty()),
            attribute_allowlist: raw.attribute_allowlist,
        })
    }

    pub fn accepts(&self, signal: OtlpSignal) -> bool {
        self.signals.contains(&signal)
    }
}

impl<'de> serde::Deserialize<'de> for OtlpConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = OtlpConfigRaw::deserialize(deserializer)?;
        Self::try_from_raw(raw).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<PluginConfigEntry> for OtlpConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Otlp")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decode_defaults() {
        let cfg: OtlpConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(cfg.listen_address_grpc, "0.0.0.0:4317");
        assert_eq!(cfg.listen_address_http, "0.0.0.0:4318");
        assert!(cfg.accepts(OtlpSignal::Traces));
        assert!(cfg.accepts(OtlpSignal::Logs));
        assert!(cfg.accepts(OtlpSignal::Metrics));
        assert!(cfg.attribute_allowlist.is_none());
    }

    #[test]
    fn reject_empty_signals() {
        let err = serde_json::from_value::<OtlpConfig>(json!({ "signals": [] })).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn reject_unknown_signal() {
        let err =
            serde_json::from_value::<OtlpConfig>(json!({ "signals": ["spans"] })).unwrap_err();
        assert!(err.to_string().contains("spans") || err.to_string().contains("unknown"));
    }

    #[test]
    fn deny_unknown_keys() {
        let err = serde_json::from_value::<OtlpConfig>(json!({ "batch_size": 1 })).unwrap_err();
        assert!(err.to_string().contains("unknown") || err.to_string().contains("batch_size"));
    }

    #[test]
    fn empty_allowlist_is_drop_all() {
        let cfg: OtlpConfig = serde_json::from_value(json!({ "attribute_allowlist": [] })).unwrap();
        assert_eq!(cfg.attribute_allowlist.as_deref(), Some(&[][..]));
    }
}
