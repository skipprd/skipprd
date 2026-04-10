use async_trait::async_trait;

use super::athena::{AwsAthena, DataSinkAthenaPluginConfig};
use crate::discover::OutputMetadata;
use crate::helpers::configuration::GlueSchemaSinkConfig;
use crate::plugins::traits::SchemaSink;

/// Schema sink backed by AWS Glue catalog, reusing existing Athena/Glue DDL logic.
///
/// Merges `glue_database_name` from the schema sink config with the
/// associated data sink's Athena config (s3_bucket, s3_prefix, workgroup,
/// results bucket). Falls back to env vars when no data sink config is
/// available (e.g. env-only deployments).
pub struct GlueSchemaSink {
    config: DataSinkAthenaPluginConfig,
}

impl GlueSchemaSink {
    /// Build from a `GlueSchemaSinkConfig` plus an optional associated
    /// Athena data sink config. When `athena_config` is `None`, the S3
    /// and workgroup fields fall back to environment variables.
    pub fn new(
        glue_config: GlueSchemaSinkConfig,
        athena_config: Option<DataSinkAthenaPluginConfig>,
    ) -> Self {
        use crate::helpers::configuration::Config;

        let base = athena_config.unwrap_or_else(|| DataSinkAthenaPluginConfig {
            format: None,
            s3_bucket: Config::getenv("DATA_OUTPUT_S3_BUCKET", ""),
            s3_prefix: Config::getenv("DATA_OUTPUT_S3_PREFIX", ""),
            athena_workgroup_name: Config::getenv("DATA_OUTPUT_ATHENA_WORKGROUP_NAME", ""),
            glue_database_name: String::new(),
            athena_results_s3_bucket: Config::getenv("DATA_OUTPUT_ATHENA_RESULTS_S3_BUCKET", ""),
        });
        let config = DataSinkAthenaPluginConfig {
            glue_database_name: glue_config.glue_database_name,
            ..base
        };
        Self { config }
    }
}

#[async_trait]
impl SchemaSink for GlueSchemaSink {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), std::io::Error> {
        AwsAthena::create_or_update_schema_with_config(namespace, metadata, self.config.clone())
            .await;
        Ok(())
    }
}
