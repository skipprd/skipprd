use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use serde_derive::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::buffer::compaction_transaction::{
    SinkGroupingSupport, SinkRetrySemantics, SinkWriteSemantics,
};
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
    pub idempotency_key: String,
    pub wal_refs: Vec<crate::runtime_plugins::protocol::RuntimeWalPartRef>,
    pub write_semantics: crate::buffer::compaction_transaction::SinkWriteSemantics,
    pub schema_fingerprint: String,
    pub cdc_ctx: Option<&'a SyncContext>,
    pub source_contract: Option<&'a SourceNamespaceContract>,
}

impl SinkWriteContext<'_> {
    pub fn is_grouped(&self) -> bool {
        !self.wal_refs.is_empty()
    }

    pub fn validate_grouped<W: SinkWriteSupport>(&self) -> Result<(), SinkWriteRejection> {
        if !self.is_grouped() {
            return Ok(());
        }
        if self.idempotency_key.is_empty() {
            return Err(SinkWriteRejection::MissingIdempotencyKey);
        }
        if W::GROUPING == SinkGroupingSupport::None {
            return Err(SinkWriteRejection::UnsupportedGrouping {
                grouping: W::GROUPING,
            });
        }
        if matches!(self.write_semantics, SinkWriteSemantics::ExactOnce) && !W::EXACT_ONCE_ALLOWED {
            return Err(SinkWriteRejection::UnsupportedExactOnce);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SinkWriteOutcome {
    Applied,
    AlreadyApplied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SinkWriteRejection {
    MissingIdempotencyKey,
    UnsupportedGrouping { grouping: SinkGroupingSupport },
    UnsupportedExactOnce,
}

impl std::fmt::Display for SinkWriteRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingIdempotencyKey => write!(f, "grouped sink write missing idempotency key"),
            Self::UnsupportedGrouping { grouping } => {
                write!(f, "sink does not support grouped writes: {grouping:?}")
            }
            Self::UnsupportedExactOnce => {
                write!(f, "sink does not support exact-once grouped writes")
            }
        }
    }
}

impl std::error::Error for SinkWriteRejection {}

pub trait SinkWriteSupport: 'static {
    const RETRY: SinkRetrySemantics;
    const GROUPING: SinkGroupingSupport;
    const EXACT_ONCE_ALLOWED: bool;
}

pub struct DeterministicObjectOverwrite;
impl SinkWriteSupport for DeterministicObjectOverwrite {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::DeterministicOverwrite;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::CdcEncodedBatches;
    const EXACT_ONCE_ALLOWED: bool = false;
}

pub struct TransactionalTableCommit;
impl SinkWriteSupport for TransactionalTableCommit {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::TransactionalIdempotent;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::FinalStateBatches;
    const EXACT_ONCE_ALLOWED: bool = true;
}

pub struct FinalStateIdempotentApply;
impl SinkWriteSupport for FinalStateIdempotentApply {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::FinalStateIdempotent;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::FinalStateBatches;
    const EXACT_ONCE_ALLOWED: bool = true;
}

pub struct AtLeastOnceMessageDelivery;
impl SinkWriteSupport for AtLeastOnceMessageDelivery {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::AtLeastOnce;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::CdcEncodedBatches;
    const EXACT_ONCE_ALLOWED: bool = false;
}

pub struct NonRetryableDebugOutput;
impl SinkWriteSupport for NonRetryableDebugOutput {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::NonRetryable;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::None;
    const EXACT_ONCE_ALLOWED: bool = false;
}

pub struct SftpAtomicRename;
impl SinkWriteSupport for SftpAtomicRename {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::DeterministicOverwrite;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::CdcEncodedBatches;
    const EXACT_ONCE_ALLOWED: bool = false;
}

pub struct SftpAtLeastOnce;
impl SinkWriteSupport for SftpAtLeastOnce {
    const RETRY: SinkRetrySemantics = SinkRetrySemantics::AtLeastOnce;
    const GROUPING: SinkGroupingSupport = SinkGroupingSupport::CdcEncodedBatches;
    const EXACT_ONCE_ALLOWED: bool = false;
}

pub trait SinkSpec: 'static {
    const NAME: &'static str;
    const CAPABILITY: SinkCapability;
    type WriteSupport: SinkWriteSupport;
}

pub trait HasSinkSpec {
    type Spec: SinkSpec;
}

pub trait SchemaSinkSpec: 'static {
    const NAME: &'static str;
}

pub trait HasSchemaSinkSpec {
    type Spec: SchemaSinkSpec;
}

pub struct ConfiguredSink<S: SinkSpec, P> {
    pub plugin: P,
    _spec: PhantomData<S>,
}

impl<S: SinkSpec, P> ConfiguredSink<S, P> {
    pub fn new(plugin: P) -> Self {
        Self {
            plugin,
            _spec: PhantomData,
        }
    }
}

#[macro_export]
macro_rules! declare_sink_spec {
    ($spec:ident, $plugin:ty, $capability:path, $support:ty) => {
        pub struct $spec;

        impl $crate::plugins::SinkSpec for $spec {
            const NAME: &'static str = $capability.name;
            const CAPABILITY: $crate::plugins::cdc::SinkCapability = $capability;
            type WriteSupport = $support;
        }

        impl $crate::plugins::HasSinkSpec for $plugin {
            type Spec = $spec;
        }
    };
}

#[macro_export]
macro_rules! declare_schema_sink_spec {
    ($spec:ident, $plugin:ty, $name:expr) => {
        pub struct $spec;

        impl $crate::plugins::SchemaSinkSpec for $spec {
            const NAME: &'static str = $name;
        }

        impl $crate::plugins::HasSchemaSinkSpec for $plugin {
            type Spec = $spec;
        }
    };
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

    #[test]
    fn grouped_context_validation_enforces_support_marker() {
        let ctx = SinkWriteContext {
            filename: "namespace=users-c=abc".to_string(),
            compaction_id: "abc".to_string(),
            idempotency_key: "abc".to_string(),
            wal_refs: vec![crate::runtime_plugins::protocol::RuntimeWalPartRef {
                segment_id: "seg-1".to_string(),
                source: "local".to_string(),
                start: 0,
                len: 10,
                sink_ref: "primary".to_string(),
                namespace: "users".to_string(),
                partition: String::new(),
                time: None,
                shard: String::new(),
                cdc_meta_hash: None,
            }],
            write_semantics: SinkWriteSemantics::IdempotentAtLeastOnce,
            schema_fingerprint: "schema".to_string(),
            cdc_ctx: None,
            source_contract: None,
        };

        assert!(ctx
            .validate_grouped::<DeterministicObjectOverwrite>()
            .is_ok());
        assert!(matches!(
            ctx.validate_grouped::<NonRetryableDebugOutput>(),
            Err(SinkWriteRejection::UnsupportedGrouping { .. })
        ));
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

    async fn sync_with_context_result(
        &self,
        stream: SendableRecordBatchStream,
        ctx: SinkWriteContext<'_>,
    ) -> Result<SinkWriteOutcome, std::io::Error> {
        self.sync_with_context(stream, ctx).await?;
        Ok(SinkWriteOutcome::Applied)
    }

    /// Return the compile-time capability descriptor for this sink.
    fn capability(&self) -> &'static SinkCapability;

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
