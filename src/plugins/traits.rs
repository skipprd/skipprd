use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use serde_derive::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::discover::OutputMetadata;
use crate::plugins::cdc::{SinkCapability, SourceCapability, SyncContext};
use crate::plugins::source_contract::SourceNamespaceContract;
use crate::plugins::source_sync::SourceSyncContext;

#[derive(Clone, Copy, Debug)]
pub enum SourceCdcContract {
    None,
    Configurable {
        mode: SourceCdcMode,
        capability: &'static SourceCapability,
    },
}

impl SourceCdcContract {
    pub fn capability(self) -> Option<&'static SourceCapability> {
        match self {
            Self::Configurable { mode, capability } if mode.includes_cdc_stream() => {
                Some(capability)
            }
            Self::Configurable { .. } | Self::None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCdcMode {
    /// Bounded read only; do not emit CDC metadata or consume a change stream.
    #[default]
    Snapshot,
    /// Initial snapshot on first run, then resume from CDC checkpoints thereafter.
    SnapshotThenCdc,
    /// No initial snapshot; start or resume from the source's CDC stream only.
    CdcOnly,
}

impl SourceCdcMode {
    pub fn includes_cdc_stream(self) -> bool {
        matches!(self, Self::SnapshotThenCdc | Self::CdcOnly)
    }

    pub fn includes_initial_snapshot(self) -> bool {
        matches!(self, Self::Snapshot | Self::SnapshotThenCdc)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceOnceContract {
    /// `sync()` returns after the current bounded snapshot/read is complete.
    Finite,
    /// `sync()` may stream indefinitely; runtime `--once` idle supervision is required.
    HostIdleBounded,
    /// The plugin has its own idle/exhaustion condition in addition to host supervision.
    PluginIdleBounded,
}

#[derive(Clone, Copy, Debug)]
pub struct SourceExecutionContract {
    pub cdc: SourceCdcContract,
    pub once: SourceOnceContract,
}

impl SourceExecutionContract {
    pub fn finite() -> Self {
        Self {
            cdc: SourceCdcContract::None,
            once: SourceOnceContract::Finite,
        }
    }

    pub fn stream(once: SourceOnceContract) -> Self {
        Self {
            cdc: SourceCdcContract::None,
            once,
        }
    }

    pub fn configurable_cdc(
        mode: SourceCdcMode,
        capability: &'static SourceCapability,
        once: SourceOnceContract,
    ) -> Self {
        Self {
            cdc: SourceCdcContract::Configurable { mode, capability },
            once,
        }
    }
}

/// Reads records from an external system and feeds them into the pipeline.
#[async_trait]
pub trait DataSource: Send + Sync {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error>;

    /// Return this source's typed execution contract.
    ///
    /// This is intentionally required for every source connector so new plugins
    /// must declare CDC semantics and `--once` termination behavior at compile time.
    fn execution_contract(&self) -> SourceExecutionContract;

    /// Return the active CDC capability descriptor for this source.
    fn capability(&self) -> Option<&'static SourceCapability> {
        self.execution_contract().cdc.capability()
    }

    /// Per-namespace extraction contracts (write policy, keys, cursors).
    fn source_namespace_contracts(&self) -> Vec<SourceNamespaceContract> {
        vec![]
    }
}

/// Context passed to sinks for a single compacted batch.
#[derive(Clone, Debug)]
pub struct SinkWriteContext<'a> {
    pub filename: String,
    pub compaction_id: String,
    pub cdc_ctx: Option<&'a SyncContext>,
    pub source_contract: Option<&'a SourceNamespaceContract>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::cdc::source_capabilities;

    #[test]
    fn source_cdc_mode_uses_snake_case_config_strings() {
        let mode: SourceCdcMode = serde_json::from_str("\"snapshot_then_cdc\"").unwrap();
        assert_eq!(mode, SourceCdcMode::SnapshotThenCdc);

        let mode: SourceCdcMode = serde_json::from_str("\"cdc_only\"").unwrap();
        assert_eq!(mode, SourceCdcMode::CdcOnly);
    }

    #[test]
    fn source_cdc_contract_exposes_capability_only_for_streaming_modes() {
        let snapshot = SourceExecutionContract::configurable_cdc(
            SourceCdcMode::Snapshot,
            &source_capabilities::POSTGRES,
            SourceOnceContract::Finite,
        );
        assert!(snapshot.cdc.capability().is_none());

        let cdc = SourceExecutionContract::configurable_cdc(
            SourceCdcMode::SnapshotThenCdc,
            &source_capabilities::POSTGRES,
            SourceOnceContract::HostIdleBounded,
        );
        assert!(cdc.cdc.capability().is_some());
    }
}

/// Writes record batches to a destination (S3, disk, database, etc.).
///
/// This trait handles the DATA plane only. Schema/DDL operations belong
/// to [`SchemaSink`]. The schema_sink pairing is declared on the config
/// entry, not on this trait.
#[async_trait]
pub trait DataSink: Send + Sync {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&SyncContext>,
    ) -> Result<(), std::io::Error>;

    async fn sync_with_context(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<(), std::io::Error> {
        self.sync(stream, ctx.filename, ctx.cdc_ctx).await
    }

    /// Return the compile-time capability descriptor for this sink.
    /// Default returns `None` for backward compatibility with existing
    /// connectors that have not yet declared capabilities.
    fn capability(&self) -> Option<&'static SinkCapability> {
        None
    }

    async fn install_schema_state(
        &self,
        _schema_version: u64,
        _namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}

/// Creates or updates schema definitions at a destination (Glue catalog,
/// SQL DDL, REST API, Iceberg catalog, etc.).
///
/// Paired with a [`DataSink`] via the sink's config entry. The schema sync
/// background worker calls this independently of data writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaSyncRequest<'a> {
    pub namespace: &'a str,
    pub compaction_id: &'a str,
    pub source_contract: Option<&'a SourceNamespaceContract>,
}

#[async_trait]
pub trait SchemaSink: Send + Sync {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), std::io::Error>;

    async fn sync_schema_request(
        &self,
        request: SchemaSyncRequest<'_>,
        metadata: &OutputMetadata,
    ) -> Result<(), std::io::Error> {
        let _ = request.source_contract;
        self.sync_schema(request.namespace, metadata).await
    }

    async fn install_schema_state(
        &self,
        _schema_version: u64,
        _namespaces: &BTreeMap<String, OutputMetadata>,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}

/// Reads schema definitions from an external catalog or system.
///
/// Not yet implemented -- trait is defined to establish the contract.
///
/// Example future implementations:
///
/// - **GlueSchemaSource**: reads table/column definitions from AWS Glue
///   (`get_table`, `get_database`) for diffing before updates.
/// - **PostgresSchemaSource**: reads `information_schema.columns` to
///   discover current table shapes.
/// - **IcebergSchemaSource**: reads Iceberg table metadata from a REST catalog.
/// - **ParquetSchemaSource**: reads Arrow schema from a Parquet file footer.
///
/// Example implementation sketch:
///
/// ```ignore
/// pub struct GlueSchemaSource {
///     glue_client: aws_sdk_glue::Client,
///     database: String,
/// }
///
/// #[async_trait::async_trait]
/// impl SchemaSource for GlueSchemaSource {
///     async fn read_schema(
///         &self,
///         namespace: &str,
///     ) -> Result<Option<OutputMetadata>, std::io::Error> {
///         let resp = self.glue_client.get_table()
///             .database_name(&self.database)
///             .name(namespace)
///             .send().await
///             .map_err(|e| std::io::Error::other(e.to_string()))?;
///         todo!("map resp.table().columns() -> OutputMetadata")
///     }
/// }
/// ```
#[async_trait]
pub trait SchemaSource: Send + Sync {
    async fn read_schema(&self, namespace: &str) -> Result<Option<OutputMetadata>, std::io::Error>;
}
