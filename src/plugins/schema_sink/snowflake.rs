use async_trait::async_trait;

use crate::discover::OutputMetadata;
use crate::helpers::configuration::DataOutputSnowflakePluginConfig;
use crate::plugins::data_sink::snowflake::DataOutputSnowflakePlugin;
use crate::plugins::traits::SchemaSink;
/// Schema sink backed by Snowflake DDL, delegating to the existing
/// `DataOutputSnowflakePlugin::sync_schema` inherent method.
pub struct SnowflakeSchemaSink {
    inner: DataOutputSnowflakePlugin,
}

impl SnowflakeSchemaSink {
    pub async fn new(config: DataOutputSnowflakePluginConfig) -> Self {
        let inner =
            DataOutputSnowflakePlugin::new_with_config("_schema_sink".into(), config).await;
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
