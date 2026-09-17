use std::sync::Arc;

use async_trait::async_trait;
use serde_derive::Deserialize;

use crate::helpers::plugin_config::PluginConfigEntry;
use skippr_runtime_sdk::plugins::DataSource;
use skippr_runtime_sdk::source_compat::SourceSyncContext;

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

    pub fn with_runtime_config(config: DataSourcePcapPluginConfig) -> Self {
        Self { _config: config }
    }
}

#[async_trait]
impl DataSource for DataSourcePcapPlugin {
    async fn sync(&mut self, _ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        Err(std::io::Error::other(
            "pcap support not compiled -- enable the pcap feature",
        ))
    }

    fn execution_contract(&self) -> skippr_runtime_sdk::plugins::SourceExecutionContract {
        skippr_runtime_sdk::plugins::SourceExecutionContract::finite()
    }
}
