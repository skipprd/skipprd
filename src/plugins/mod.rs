pub mod cdc;
pub mod traits;

pub use traits::{
    DataSink, DataSource, RuntimeIngestRelay, SchemaSink, SchemaSource, SchemaSyncRequest,
};

/// No-op output plugin used by `discover` mode to run the input pipeline
/// without writing to any destination.
pub struct NoopOutputPlugin;

#[async_trait::async_trait]
impl DataSink for NoopOutputPlugin {
    async fn sync(
        &self,
        _stream: datafusion::execution::SendableRecordBatchStream,
        _filename: String,
        _cdc_ctx: Option<&crate::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}
