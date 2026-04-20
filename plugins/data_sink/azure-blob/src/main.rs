use std::io;

use clap::Parser;
use skippr_plugin_data_sink_azure_blob::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "AzureBlob",
        "AzureBlob",
        skippr_core::plugins::cdc::sink_capabilities::by_name("AzureBlob").map(Into::into),
        false,
        "skippr-plugin-data-sink-azure-blob",
        |install| async move {
            let cfg: DataSinkAzureBlobPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            DataSinkAzureBlobPlugin::new_with_config(
                buffer_name_for_runtime_binding(install.binding),
                cfg,
            )
            .await
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-azure-blob: {}", err);
        std::process::exit(1);
    }
}
