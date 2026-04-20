use std::io;

use clap::Parser;
use skippr_plugin_data_sink_gcs::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct GcsSinkRuntimePluginCli {}

#[tokio::main]
async fn main() {
    let _cli = GcsSinkRuntimePluginCli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "Gcs",
        "Gcs",
        skippr_core::plugins::cdc::sink_capabilities::by_name("Gcs").map(Into::into),
        false,
        "skippr-plugin-data-sink-gcs",
        |install| async move {
            let config: DataSinkGcsPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            DataSinkGcsPlugin::new_with_config(
                buffer_name_for_runtime_binding(install.binding),
                config,
            )
            .await
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-gcs: {}", err);
        std::process::exit(1);
    }
}
