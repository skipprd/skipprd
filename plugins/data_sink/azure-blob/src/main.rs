use std::io;

use clap::Parser;
use skippr::runtime_plugins::sink_stdio_entry::run_stdio_data_sink_plugin;
use skippr_plugin_runtime_link::runtime_sink_link::azure_blob::{
    DataSinkAzureBlobPlugin, DataSinkAzureBlobPluginConfig,
};

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_stdio_data_sink_plugin(
        "AzureBlob",
        "AzureBlob",
        false,
        "skippr-plugin-data-sink-azure-blob",
        |_binding, buffer, env| async move {
            let cfg: DataSinkAzureBlobPluginConfig = env.decode().map_err(io::Error::other)?;
            Ok(DataSinkAzureBlobPlugin::new_with_config(buffer, Some(cfg)).await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-azure-blob: {}", err);
        std::process::exit(1);
    }
}
