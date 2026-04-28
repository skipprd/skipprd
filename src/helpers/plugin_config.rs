use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::serdes::input_format::InputFormat;

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
        InputFormat::from(self.format().as_str())
    }

    pub fn format(&self) -> String {
        self.string_field("format")
            .unwrap_or_else(|| default_format_for_plugin(&self.plugin_name).to_string())
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

fn default_format_for_plugin(plugin_name: &str) -> &'static str {
    match plugin_name {
        "Mssql" => "row",
        "Snowflake" | "AzureBlob" | "Gcs" | "GCS" | "Sftp" | "Databricks" | "Redshift"
        | "Iceberg" => "parquet",
        _ => "json",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
}
