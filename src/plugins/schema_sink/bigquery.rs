use async_trait::async_trait;

use crate::discover::OutputMetadata;
use crate::helpers::configuration::DataSinkBigqueryPluginConfig;
use crate::plugins::traits::SchemaSink;

/// Schema sink for BigQuery DDL (ensure dataset + ensure table).
///
/// Currently a stub -- BigQuery `DataSink::sync()` handles DDL inline
/// via `ensure_dataset` + `ensure_table` as an idempotent safety net.
/// This explicit `SchemaSink` enables the schema sync background worker
/// to manage BigQuery DDL independently of data writes.
pub struct BigquerySchemaSink {
    #[allow(dead_code)]
    config: DataSinkBigqueryPluginConfig,
}

impl BigquerySchemaSink {
    pub fn new(config: DataSinkBigqueryPluginConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl SchemaSink for BigquerySchemaSink {
    async fn sync_schema(
        &self,
        _namespace: &str,
        _metadata: &OutputMetadata,
    ) -> Result<(), std::io::Error> {
        // TODO: Extract ensure_dataset + ensure_table logic from
        // DataSinkBigqueryPlugin into this method.
        Ok(())
    }
}
