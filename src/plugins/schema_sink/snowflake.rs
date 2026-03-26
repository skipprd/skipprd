use async_trait::async_trait;

use crate::discover::OutputMetadata;
use crate::helpers::configuration::DataSinkSnowflakePluginConfig;
use crate::plugins::data_sink::snowflake::DataSinkSnowflakePlugin;
use crate::plugins::traits::SchemaSink;
/// Schema sink backed by Snowflake DDL, delegating to the existing
/// `DataSinkSnowflakePlugin::sync_schema` inherent method.
pub struct SnowflakeSchemaSink {
    inner: DataSinkSnowflakePlugin,
}

impl SnowflakeSchemaSink {
    pub async fn new(config: DataSinkSnowflakePluginConfig) -> Self {
        let inner =
            DataSinkSnowflakePlugin::new_with_config("_schema_sink".into(), config).await;
        Self { inner }
    }
}

#[async_trait]
impl SchemaSink for SnowflakeSchemaSink {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), std::io::Error> {
        self.inner.sync_schema(namespace, metadata).await
    }
}
