use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::discover::OutputMetadata;
use crate::helpers::offsets::Offsets;
use crate::plugins::cdc::{SinkCapability, SourceCapability, SyncContext};
use crate::runtime_plugins::protocol::{
    RuntimeIngestPartitionBatch, RuntimeOffsetMaterializationHint,
};

pub trait RuntimeIngestRelay: Send + Sync {
    fn relay_ingest_batches(
        &self,
        batches: Vec<RuntimeIngestPartitionBatch>,
    ) -> Result<(), std::io::Error>;

    fn relay_offset_hints(
        &self,
        offsets: Vec<RuntimeOffsetMaterializationHint>,
    ) -> Result<(), std::io::Error>;
}

/// Reads records from an external system and feeds them into the pipeline.
#[async_trait]
pub trait DataSource: Send + Sync {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error>;

    /// Return the compile-time capability descriptor for this source.
    /// Default returns `None` for backward compatibility with existing
    /// connectors that have not yet declared capabilities.
    fn capability(&self) -> Option<&'static SourceCapability> {
        None
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

    /// Return the compile-time capability descriptor for this sink.
    /// Default returns `None` for backward compatibility with existing
    /// connectors that have not yet declared capabilities.
    fn capability(&self) -> Option<&'static SinkCapability> {
        None
    }

    fn runtime_ingest_relay(&self) -> Option<&dyn RuntimeIngestRelay> {
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
