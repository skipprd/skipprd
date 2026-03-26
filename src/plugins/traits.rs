use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use std::sync::Arc;

use crate::discover::OutputMetadata;
use crate::helpers::offsets::Offsets;

/// Reads records from an external system and feeds them into the pipeline.
#[async_trait]
pub trait DataSource: Send + Sync {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error>;
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
    ) -> Result<(), std::io::Error>;
}

/// Creates or updates schema definitions at a destination (Glue catalog,
/// SQL DDL, REST API, Iceberg catalog, etc.).
///
/// Paired with a [`DataSink`] via the sink's config entry. The schema sync
/// background worker calls this independently of data writes.
#[async_trait]
pub trait SchemaSink: Send + Sync {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), std::io::Error>;
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
    async fn read_schema(
        &self,
        namespace: &str,
    ) -> Result<Option<OutputMetadata>, std::io::Error>;
}
