use super::plugin_config::PluginConfigEntry;

pub type DataSourcePluginConfig = PluginConfigEntry;
pub type DataSinkPluginConfig = PluginConfigEntry;
pub type SchemaSinkConfig = PluginConfigEntry;

/// Child-process config is empty: plugins do not load skippr.yml.
/// Namespace sanitization uses `flatten_events = false`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config;

impl Config {
    pub fn new() -> Self {
        Self
    }
}

pub fn runtime_child_config() -> Config {
    Config::new()
}
