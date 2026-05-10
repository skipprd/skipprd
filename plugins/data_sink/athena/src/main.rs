use std::io;

use clap::Parser;
use skippr_plugin_data_sink_athena::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "Athena",
        "Athena",
        skippr_runtime_sdk::plugins::cdc::sink_capabilities::by_name("Athena").map(Into::into),
        false,
        "skippr-plugin-data-sink-athena",
        |install| async move {
            let cfg: DataSinkAthenaPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            Ok(DataSinkAthenaPlugin::new_with_config(
                install.context,
                install.binding,
                buffer_name_for_runtime_binding(install.binding),
                cfg,
            )
            .await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-athena: {}", err);
        std::process::exit(1);
    }
}
