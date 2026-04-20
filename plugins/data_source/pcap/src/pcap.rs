use std::sync::Arc;

use async_trait::async_trait;
use serde_derive::Deserialize;

use crate::helpers::configuration::Config;
use crate::helpers::offsets::Offsets;
use crate::helpers::plugin_config::PluginConfigEntry;
use crate::plugins::{DataSink, DataSource};

#[derive(Debug, Deserialize, Clone, Default)]
pub struct DataSourcePcapPluginConfig {}

impl TryFrom<PluginConfigEntry> for DataSourcePcapPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Pcap")
    }
}

pub struct DataSourcePcapPlugin {
    _config: DataSourcePcapPluginConfig,
}

impl DataSourcePcapPlugin {
    pub async fn new() -> Self {
        let _config: DataSourcePcapPluginConfig = match Config::get_pipeline_input_plugin_config() {
            Ok(c) => c.try_into().unwrap_or_else(|e| panic!("{}", e)),
            Err(_) => DataSourcePcapPluginConfig::default(),
        };
        Self { _config }
    }

    pub fn with_runtime_config(config: DataSourcePcapPluginConfig) -> Self {
        Self { _config: config }
    }
}

#[async_trait]
impl DataSource for DataSourcePcapPlugin {
    async fn sync(
        &mut self,
        _offsets: Arc<Offsets>,
        _output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        Err(std::io::Error::other(
            "pcap support not compiled -- enable the pcap feature",
        ))
    }
}
