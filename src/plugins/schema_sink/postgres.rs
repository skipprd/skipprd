use async_trait::async_trait;

use crate::discover::OutputMetadata;
use crate::helpers::configuration::DataSinkPostgresPluginConfig;
use crate::plugins::traits::SchemaSink;

/// Schema sink for Postgres DDL (CREATE/ALTER TABLE).
///
/// Currently a stub -- Postgres `DataSink::sync()` handles DDL inline
/// via `ensure_schema` + `ensure_table` as an idempotent safety net.
/// This explicit `SchemaSink` enables the schema sync background worker
/// to manage Postgres DDL independently of data writes.
pub struct PostgresSchemaSink {
    #[allow(dead_code)]
    config: DataSinkPostgresPluginConfig,
}

impl PostgresSchemaSink {
    pub fn new(config: DataSinkPostgresPluginConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl SchemaSink for PostgresSchemaSink {
    async fn sync_schema(
        &self,
        _namespace: &str,
        _metadata: &OutputMetadata,
    ) -> Result<(), std::io::Error> {
        // TODO: Extract ensure_schema + ensure_table logic from
        // DataSinkPostgresPlugin into this method.
        Ok(())
    }
}
