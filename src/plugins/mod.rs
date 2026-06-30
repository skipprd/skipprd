pub mod cdc;
pub mod source_contract;
pub mod source_sync;
pub mod traits;

pub use source_contract::{
    apply_runtime_source_namespace_contracts, ensure_source_contract_for_policy,
    merge_source_contracts_into_pipeline, replace_source_contracts_authoritative,
    resolved_write_policy_for_namespace, validate_active_sink_supports_contracts,
    validate_namespace_contracts, validate_write_policy_for_sink, FieldPath,
    SinkWritePolicySupport, SourceContractError, SourceNamespaceContract, SourceSemantics,
    WritePolicy, WritePolicyUnsupportedError,
};
pub use source_sync::{OffsetValidationEntry, SourcePayloadTask, SourceSyncContext};
pub use traits::{
    AtLeastOnceMessageDelivery, ConfiguredSink, DataSink, DataSource, DeterministicObjectOverwrite,
    FinalStateIdempotentApply, HasSchemaSinkSpec, HasSinkSpec, NonRetryableDebugOutput, SchemaSink,
    SchemaSinkSpec, SchemaSource, SchemaSyncRequest, SftpAtLeastOnce, SftpAtomicRename, SinkSpec,
    SinkWriteContext, SinkWriteOutcome, SinkWriteRejection, SinkWriteSemantics, SinkWriteSupport,
    SourceCdcContract, SourceCdcMode, SourceExecutionContract, SourceOnceContract,
    TransactionalTableCommit,
};

/// No-op output plugin used by `discover` mode to run the input pipeline
/// without writing to any destination.
pub struct NoopOutputPlugin;

#[async_trait::async_trait]
impl DataSink for NoopOutputPlugin {
    async fn sync(
        &self,
        stream: datafusion::execution::SendableRecordBatchStream,
        _filename: String,
        _cdc_ctx: Option<&crate::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        use futures::StreamExt;
        let mut stream = stream;
        while let Some(batch) = stream.next().await {
            let _ = batch.map_err(|e| std::io::Error::other(e.to_string()))?;
        }
        Ok(())
    }

    async fn sync_with_context(
        &self,
        stream: datafusion::execution::SendableRecordBatchStream,
        ctx: crate::plugins::SinkWriteContext<'_>,
    ) -> Result<(), std::io::Error> {
        ctx.validate_grouped::<crate::plugins::AtLeastOnceMessageDelivery>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Unsupported, e))?;
        self.sync(stream, ctx.filename, ctx.cdc_ctx).await
    }

    fn capability(&self) -> &'static crate::plugins::cdc::SinkCapability {
        &crate::plugins::cdc::sink_capabilities::STDOUT
    }
}
