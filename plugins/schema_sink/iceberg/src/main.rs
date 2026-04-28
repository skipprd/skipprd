use std::io;

use clap::Parser;
use skippr_plugin_data_sink_iceberg::*;
use skippr_runtime_sdk::sink_runtime_entry::run_runtime_schema_sink_plugin;

struct IcebergSchemaSync {
    inner: DataSinkIcebergPlugin,
}

#[async_trait::async_trait]
impl skippr_core::plugins::SchemaSink for IcebergSchemaSync {
    async fn sync_schema(
        &self,
        namespace: &str,
        metadata: &skippr_core::discover::OutputMetadata,
    ) -> Result<(), io::Error> {
        <DataSinkIcebergPlugin as skippr_core::plugins::SchemaSink>::sync_schema(
            &self.inner,
            namespace,
            metadata,
        )
        .await
    }
}

#[derive(Debug, Parser)]
struct IcebergSchemaRuntimePluginCli {}

#[tokio::main]
async fn main() {
    let _cli = IcebergSchemaRuntimePluginCli::parse();
    if let Err(err) = run_runtime_schema_sink_plugin(
        "Iceberg",
        "Iceberg",
        "skippr-plugin-schema-sink-iceberg",
        |install| async move {
            let cfg: DataSinkIcebergPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            let inner = DataSinkIcebergPlugin::new_with_config(
                install.context,
                install.binding,
                "schema".to_string(),
                cfg,
            )
            .await?;
            Ok(IcebergSchemaSync { inner })
        },
    )
    .await
    {
        eprintln!("skippr-plugin-schema-sink-iceberg: {}", err);
        std::process::exit(1);
    }
}
