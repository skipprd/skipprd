use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::serdes::input_format::InputFormat;
use crate::serdes::output_format::OutputFormat;

#[derive(Debug, Clone, PartialEq)]
pub struct PluginConfigEntry {
    pub plugin_name: String,
    pub config: Value,
}

impl PluginConfigEntry {
    pub fn plugin_name(&self) -> Option<String> {
        Some(self.plugin_name.clone())
    }

    pub fn input_format(&self) -> InputFormat {
        InputFormat::from_option(self.string_field("format").as_deref())
    }

    pub fn resolved_output_format(&self) -> Result<Option<OutputFormat>, String> {
        match crate::plugins::cdc::sink_capabilities::by_name(&self.plugin_name) {
            Some(capability) => capability.resolve_format(self.string_field("format").as_deref()),
            None => Ok(None),
        }
    }

    pub fn format(&self) -> String {
        if crate::connect::DataSink::parse(&self.plugin_name).is_some() {
            return self
                .resolved_output_format()
                .ok()
                .flatten()
                .map(|format| format.to_string())
                .unwrap_or_default();
        }
        self.string_field("format")
            .unwrap_or_else(|| InputFormat::default().as_str().to_string())
    }

    pub fn batch_size_bytes(&self) -> Option<i64> {
        self.i64_field("batch_size_bytes")
    }

    pub fn batch_size_seconds(&self) -> Option<i64> {
        self.i64_field("batch_size_seconds")
    }

    pub fn version(&self) -> Option<String> {
        self.string_field("version")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    pub fn deserialize<T: DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_value(self.config.clone()).map_err(|err| {
            format!(
                "Failed to decode config for plugin '{}': {}",
                self.plugin_name, err
            )
        })
    }

    pub fn decode_for_plugin<T: DeserializeOwned>(&self, expected: &str) -> Result<T, String> {
        if self.plugin_name != expected {
            return Err(format!(
                "config targets plugin '{}' but '{}' was expected",
                self.plugin_name, expected
            ));
        }
        self.deserialize()
    }

    pub fn with_json_field(mut self, key: &str, value: Value) -> Self {
        match &mut self.config {
            Value::Object(map) => {
                map.insert(key.to_string(), value);
            }
            Value::Null => {
                let mut map = Map::new();
                map.insert(key.to_string(), value);
                self.config = Value::Object(map);
            }
            Value::Array(_) | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                let mut map = Map::new();
                map.insert(key.to_string(), value);
                self.config = Value::Object(map);
            }
        }
        self
    }

    fn string_field(&self, key: &str) -> Option<String> {
        self.config
            .as_object()
            .and_then(|config| config.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn i64_field(&self, key: &str) -> Option<i64> {
        self.config.as_object().and_then(|config| {
            config.get(key).and_then(|value| match value {
                Value::Number(num) => num.as_i64(),
                Value::String(raw) => raw.parse::<i64>().ok(),
                _ => None,
            })
        })
    }
}

impl<'de> Deserialize<'de> for PluginConfigEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let map = HashMap::<String, Value>::deserialize(deserializer)?;
        plugin_entry_from_map(map, &[])
            .map_err(|err| <D::Error as serde::de::Error>::custom(err.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataSinkEntry {
    pub config: PluginConfigEntry,
    pub schema_sink: Option<String>,
}

impl<'de> Deserialize<'de> for DataSinkEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut map = HashMap::<String, Value>::deserialize(deserializer)?;
        let schema_sink = map
            .remove("schema_sink")
            .and_then(|value| value.as_str().map(str::to_string));
        let config = plugin_entry_from_map(map, &[])
            .map_err(|err| <D::Error as serde::de::Error>::custom(err.to_string()))?;
        Ok(Self {
            config,
            schema_sink,
        })
    }
}

fn plugin_entry_from_map(
    map: HashMap<String, Value>,
    reserved_keys: &[&str],
) -> Result<PluginConfigEntry, String> {
    let plugin_entries: Vec<(String, Value)> = map
        .into_iter()
        .filter(|(key, _)| !reserved_keys.contains(&key.as_str()))
        .collect();
    match plugin_entries.as_slice() {
        [(plugin_name, config)] => Ok(PluginConfigEntry {
            plugin_name: plugin_name.clone(),
            config: config.clone(),
        }),
        [] => Err("Expected exactly one plugin entry but found none".to_string()),
        _ => Err("Expected exactly one plugin entry but found multiple".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::serdes::output_format::OutputFormat;

    use super::PluginConfigEntry;

    #[test]
    fn version_reads_optional_plugin_field() {
        let entry = PluginConfigEntry {
            plugin_name: "Athena".to_string(),
            config: json!({
                "version": "0.1.1",
                "s3_bucket": "bucket"
            }),
        };

        assert_eq!(entry.version().as_deref(), Some("0.1.1"));
    }

    #[test]
    fn version_ignores_blank_values() {
        let entry = PluginConfigEntry {
            plugin_name: "Athena".to_string(),
            config: json!({
                "version": "   "
            }),
        };

        assert_eq!(entry.version(), None);
    }

    #[test]
    fn skipprlake_does_not_accept_format() {
        let src = include_str!("plugin_config.rs");
        assert!(
            !src.contains("is_plugin_name(name) => \"parquet\""),
            "SkipprLake must not default format to parquet"
        );
        let entry = PluginConfigEntry {
            plugin_name: skippr_iceberg_catalog::SkipprLakeConfig::PLUGIN_NAME.to_string(),
            config: serde_json::json!({
                "warehouse": "file:///tmp/warehouse",
                "catalog_table": "cat",
                "object_store": { "type": "file" },
                "table_namespace": "bronze"
            }),
        };
        assert_eq!(entry.resolved_output_format().unwrap(), None);
        assert_eq!(entry.format(), "");
    }

    #[test]
    fn athena_defaults_parquet_and_rejects_jsonl() {
        let entry = PluginConfigEntry {
            plugin_name: crate::plugins::cdc::sink_capabilities::ATHENA
                .name
                .to_string(),
            config: serde_json::json!({
                "s3_bucket": "output",
                "s3_prefix": "warehouse",
                "athena_workgroup_name": "primary",
                "athena_results_s3_bucket": "results"
            }),
        };
        assert_eq!(
            entry.resolved_output_format().unwrap(),
            Some(OutputFormat::Parquet)
        );

        let parquet = PluginConfigEntry {
            plugin_name: crate::plugins::cdc::sink_capabilities::ATHENA
                .name
                .to_string(),
            config: serde_json::json!({ "format": "parquet" }),
        };
        assert_eq!(
            parquet.resolved_output_format().unwrap(),
            Some(OutputFormat::Parquet)
        );

        let rejected = PluginConfigEntry {
            plugin_name: crate::plugins::cdc::sink_capabilities::ATHENA
                .name
                .to_string(),
            config: serde_json::json!({ "format": "jsonl" }),
        };
        let err = rejected.resolved_output_format().unwrap_err();
        assert!(err.contains("does not support format 'jsonl'"));
    }

    #[test]
    fn athena_iceberg_does_not_accept_format() {
        let entry = PluginConfigEntry {
            plugin_name: crate::plugins::cdc::sink_capabilities::ATHENA_ICEBERG
                .name
                .to_string(),
            config: serde_json::json!({
                "warehouse": "s3://lake/",
                "glue_database_name": "analytics",
                "athena_workgroup_name": "primary",
                "athena_results_s3_bucket": "results"
            }),
        };
        assert_eq!(entry.resolved_output_format().unwrap(), None);
        assert_eq!(entry.format(), "");
    }

    #[test]
    fn duckdb_does_not_accept_format() {
        let entry = PluginConfigEntry {
            plugin_name: crate::plugins::cdc::sink_capabilities::DUCKDB
                .name
                .to_string(),
            config: serde_json::json!({
                "warehouse": "file:///tmp/lake",
                "table_namespace": "bronze"
            }),
        };
        assert_eq!(entry.resolved_output_format().unwrap(), None);
        assert_eq!(entry.format(), "");
    }

    #[test]
    fn file_omitted_format_is_parquet() {
        let entry = PluginConfigEntry {
            plugin_name: "File".to_string(),
            config: json!({}),
        };
        assert_eq!(
            entry.resolved_output_format().unwrap(),
            Some(OutputFormat::Parquet)
        );
    }

    #[test]
    fn file_accepts_jsonl() {
        let entry = PluginConfigEntry {
            plugin_name: "File".to_string(),
            config: json!({ "format": "jsonl" }),
        };
        assert_eq!(
            entry.resolved_output_format().unwrap(),
            Some(OutputFormat::Jsonl)
        );
    }

    #[test]
    fn postgres_rejects_configured_format() {
        let entry = PluginConfigEntry {
            plugin_name: "Postgres".to_string(),
            config: json!({ "format": "jsonl" }),
        };
        let err = entry.resolved_output_format().unwrap_err();
        assert!(err.contains("writes rows into tables"));
        assert!(err.contains("Remove `format: jsonl`"));
    }
}
