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
pub use data_source::dynamodb as dynamodb_input;
pub use data_source::file as file_input;
pub use data_source::kinesis as kinesis_input;
pub use data_source::mssql as mssql_input;
pub use data_source::mysql as mysql_input;
pub use data_source::s3 as s3_input;
pub use data_source::sqs as sqs_input;
pub use data_source::http_client as http_client_input;
pub use data_source::http_server as http_server_input;
pub use data_source::pcap as pcap_input;
pub use data_source::stdin as stdin_input;
pub use data_source::mongodb as mongodb_input;
pub use data_source::eventbridge as eventbridge_input;
pub use data_source::sns as sns_input;
pub use data_source::mqtt as mqtt_input;
pub use data_source::sftp as sftp_input;
pub use data_source::postgres as postgres_input;
pub use data_source::redshift as redshift_input;
pub use data_source::amqp as amqp_input;
pub use data_source::kafka as kafka_input;
pub use data_source::websocket as websocket_input;
pub use data_source::statsd as statsd_input;
pub use data_source::socket as socket_input;
pub use data_sink::stdout as stdout_output;
pub use data_sink::azure_blob as azure_blob_output;
pub use data_sink::gcs as gcs_output;
pub use data_sink::synapse as synapse_output;
pub use data_sink::sftp as sftp_output;
pub use data_sink::amqp as amqp_output;
pub use data_sink::databricks as databricks_output;
pub use data_source::clickhouse as clickhouse_input;
pub use data_source::delta_lake as delta_lake_input;
pub use data_source::motherduck as motherduck_input;
pub use data_sink::clickhouse as clickhouse_output;
pub use data_sink::redshift as redshift_output;
pub use data_sink::motherduck as motherduck_output;
pub use util::parquet as parquet_util;

pub use traits::{DataSink, DataSource, SchemaSink, SchemaSource};

/// Build a `SchemaSink` from a `DataSinkPluginConfig` (legacy fallback).
/// Returns `None` for plugins that don't need upfront DDL.
pub async fn build_schema_sync_plugin(
    config: crate::helpers::configuration::DataSinkPluginConfig,
) -> Option<Box<dyn SchemaSink + Send + Sync>> {
    use crate::helpers::configuration::DataSinkPluginConfig;
    match config {
        DataSinkPluginConfig::Athena(c) => {
            let glue_config = crate::helpers::configuration::GlueSchemaSinkConfig {
                glue_database_name: c.glue_database_name.clone(),
            };
            Some(Box::new(schema_sink::glue::GlueSchemaSink::new(glue_config, Some(c))))
        }
        DataSinkPluginConfig::Snowflake(c) => {
            let plugin = data_sink::snowflake::DataSinkSnowflakePlugin::new_with_config(
                String::new(),
                c,
            )
            .await;
            Some(Box::new(plugin))
        }
        DataSinkPluginConfig::Postgres(c) => {
            let plugin = data_sink::postgres::DataSinkPostgresPlugin::new_with_config(
                String::new(),
                c,
            )
            .await;
            Some(Box::new(plugin))
        }
        DataSinkPluginConfig::Bigquery(c) => {
            let plugin = data_sink::bigquery::DataSinkBigqueryPlugin::new_with_config(
                String::new(),
                c,
            )
            .await;
            Some(Box::new(plugin))
        }
        DataSinkPluginConfig::Clickhouse(c) => {
            let plugin = data_sink::clickhouse::DataSinkClickhousePlugin::new_with_config(
                String::new(),
                c,
            )
            .await;
            Some(Box::new(plugin))
        }
        DataSinkPluginConfig::Motherduck(c) => {
            let plugin = data_sink::motherduck::DataSinkMotherduckPlugin::new_with_config(
                String::new(),
                c,
            )
            .await;
            Some(Box::new(plugin))
        }
        DataSinkPluginConfig::Redshift(c) => {
            let plugin = data_sink::redshift::DataSinkRedshiftPlugin::new_with_config(
                String::new(),
                c,
            )
            .await;
            Some(Box::new(plugin))
        }
        _ => None,
    }
}

/// Resolve the Athena data sink config from an `DataSinkPluginConfig`, if it
/// is an `Athena` variant. Used to pass S3/workgroup fields to
/// `GlueSchemaSink` so Glue tables get the correct location.
fn extract_athena_config(
    cfg: &crate::helpers::configuration::DataSinkPluginConfig,
) -> Option<data_sink::athena::DataSinkAthenaPluginConfig> {
    match cfg {
        crate::helpers::configuration::DataSinkPluginConfig::Athena(c) => Some(c.clone()),
        _ => None,
    }
}

/// Build a `SchemaSink` from a `SchemaSinkConfig`.
///
/// `data_sink_config` is the associated data sink's `DataSinkPluginConfig`
/// (primary or deadletter). When present and the data sink is Athena, its
/// S3/workgroup fields are forwarded to the `GlueSchemaSink` so Glue
/// tables reference the correct S3 location.
pub async fn build_schema_sink(
    config: crate::helpers::configuration::SchemaSinkConfig,
    data_sink_config: Option<&crate::helpers::configuration::DataSinkPluginConfig>,
) -> Box<dyn SchemaSink + Send + Sync> {
    use crate::helpers::configuration::SchemaSinkConfig;
    match config {
        SchemaSinkConfig::Glue(c) => {
            let athena_cfg = data_sink_config.and_then(extract_athena_config);
            Box::new(schema_sink::glue::GlueSchemaSink::new(c, athena_cfg))
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
