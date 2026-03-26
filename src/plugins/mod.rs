pub mod traits;

pub mod data_source;
pub mod data_sink;
pub mod schema_sink;
pub mod schema_source;
pub mod util;

// Backward-compatible re-exports at the old module paths.
pub use data_sink::athena;
pub use data_sink::bigquery as bigquery_output;
pub use data_sink::file as file_output;
pub use data_sink::postgres as postgres_output;
pub use data_sink::s3 as s3_output;
pub use data_sink::snowflake as snowflake_output;
pub use data_source::file as file_input;
pub use data_source::mssql as mssql_input;
pub use data_source::s3 as s3_input;
pub use util::parquet as parquet_util;

pub use traits::{DataSink, DataSource, SchemaSink, SchemaSource};

/// Build a `SchemaSink` from an `OutputPluginConfig` (legacy fallback).
/// Returns `None` for plugins that don't need upfront DDL.
pub async fn build_schema_sync_plugin(
    config: crate::helpers::configuration::OutputPluginConfig,
) -> Option<Box<dyn SchemaSink + Send + Sync>> {
    use crate::helpers::configuration::OutputPluginConfig;
    match config {
        OutputPluginConfig::Athena(c) => {
            let glue_config = crate::helpers::configuration::GlueSchemaSinkConfig {
                glue_database_name: c.glue_database_name.clone(),
            };
            Some(Box::new(schema_sink::glue::GlueSchemaSink::new(glue_config)))
        }
        OutputPluginConfig::Snowflake(c) => Some(Box::new(
            schema_sink::snowflake::SnowflakeSchemaSink::new(c).await,
        )),
        _ => None,
    }
}

/// Build a `SchemaSink` from a `SchemaSinkConfig`.
pub async fn build_schema_sink(
    config: crate::helpers::configuration::SchemaSinkConfig,
) -> Box<dyn SchemaSink + Send + Sync> {
    use crate::helpers::configuration::SchemaSinkConfig;
    match config {
        SchemaSinkConfig::Glue(c) => Box::new(schema_sink::glue::GlueSchemaSink::new(c)),
        SchemaSinkConfig::Snowflake(c) => {
            Box::new(schema_sink::snowflake::SnowflakeSchemaSink::new(c).await)
        }
        SchemaSinkConfig::Postgres(c) => {
            Box::new(schema_sink::postgres::PostgresSchemaSink::new(c))
        }
        SchemaSinkConfig::Bigquery(c) => {
            Box::new(schema_sink::bigquery::BigquerySchemaSink::new(c))
        }
    }
}

/// No-op output plugin used by `discover` mode to run the input pipeline
/// without writing to any destination.
pub struct NoopOutputPlugin;

#[async_trait::async_trait]
impl DataSink for NoopOutputPlugin {
    async fn sync(
        &self,
        _stream: datafusion::execution::SendableRecordBatchStream,
        _filename: String,
    ) -> Result<(), std::io::Error> {
        Ok(())
    }
}
