use std::io;

use async_trait::async_trait;
use clap::Parser;
use skippr::discover::OutputMetadata;
use skippr::plugins::SchemaSink;
use skippr::runtime_plugins::sink_stdio_entry::run_stdio_schema_sink_plugin;
use skippr_plugin_runtime_link::runtime_sink_link::athena::{AwsAthena, DataSinkAthenaPluginConfig};

struct GlueAthenaSchemaSync {
    config: DataSinkAthenaPluginConfig,
}

#[async_trait]
impl SchemaSink for GlueAthenaSchemaSync {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &OutputMetadata,
    ) -> Result<(), io::Error> {
        AwsAthena::create_or_update_schema_with_config(namespace, metadata, self.config.clone())
            .await;
        Ok(())
    }
}

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_stdio_schema_sink_plugin(
        "Athena",
        "Glue",
        "skippr-plugin-schema-sink-glue",
        |_binding, _buffer, env| async move {
            let cfg: DataSinkAthenaPluginConfig = env.decode().map_err(io::Error::other)?;
            Ok(GlueAthenaSchemaSync { config: cfg })
        },
    )
    .await
    {
        eprintln!("skippr-plugin-schema-sink-glue: {}", err);
        std::process::exit(1);
    }
}
