use std::io;

use async_trait::async_trait;
use clap::Parser;
use skippr_runtime_sdk::discover::OutputMetadata;
use skippr_runtime_sdk::plugins::SchemaSink;
use skippr_plugin_data_sink_athena::*;
use skippr_runtime_sdk::protocol::{RuntimeBinding, RuntimeExecutionContext};
use skippr_runtime_sdk::sink_runtime_entry::run_runtime_schema_sink_plugin;

struct GlueAthenaSchemaSync {
    context: RuntimeExecutionContext,
    binding: RuntimeBinding,
    config: DataSinkAthenaPluginConfig,
}

#[async_trait]
impl SchemaSink for GlueAthenaSchemaSync {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), io::Error> {
        AwsAthena::create_or_update_schema_with_config(
            namespace,
            metadata,
            &self.context,
            self.binding,
            self.config.clone(),
        )
        .await
    }
}

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_runtime_schema_sink_plugin(
        "Athena",
        "Glue",
        "skippr-plugin-schema-sink-glue",
        |install| async move {
            let cfg: DataSinkAthenaPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            Ok(GlueAthenaSchemaSync {
                context: install.context,
                binding: install.binding,
                config: cfg,
            })
        },
    )
    .await
    {
        eprintln!("skippr-plugin-schema-sink-glue: {}", err);
        std::process::exit(1);
    }
}
