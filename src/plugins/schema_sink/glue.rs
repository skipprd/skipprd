use async_trait::async_trait;

use crate::discover::OutputMetadata;
use crate::helpers::configuration::GlueSchemaSinkConfig;
use crate::plugins::data_sink::athena::{AwsAthena, DataOutputAwsAthenaPluginConfig};
use crate::plugins::traits::SchemaSink;

/// Schema sink backed by AWS Glue catalog, reusing existing Athena/Glue DDL logic.
///
/// The `glue_database_name` is the only required field. The remaining
/// Athena-specific fields (workgroup, results bucket) are populated from
/// env vars or defaults since schema DDL only touches the Glue catalog.
pub struct GlueSchemaSink {
    config: DataOutputAwsAthenaPluginConfig,
}

impl GlueSchemaSink {
    pub fn new(glue_config: GlueSchemaSinkConfig) -> Self {
        use crate::helpers::configuration::Config;

        let config = DataOutputAwsAthenaPluginConfig {
            format: None,
            s3_bucket: Config::getenv("DATA_OUTPUT_S3_BUCKET", ""),
            s3_prefix: Config::getenv("DATA_OUTPUT_S3_PREFIX", ""),
            athena_workgroup_name: Config::getenv("DATA_OUTPUT_ATHENA_WORKGROUP_NAME", ""),
            glue_database_name: glue_config.glue_database_name,
            athena_results_s3_bucket: Config::getenv(
                "DATA_OUTPUT_ATHENA_RESULTS_S3_BUCKET",
                "",
            ),
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
