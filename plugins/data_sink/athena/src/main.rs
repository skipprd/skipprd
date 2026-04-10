use std::io;

use clap::Parser;
use skippr::runtime_plugins::sink_stdio_entry::run_stdio_data_sink_plugin;
use skippr_plugin_runtime_link::runtime_sink_link::athena::DataSinkAthenaPlugin;
use skippr_plugin_runtime_link::runtime_sink_link::athena::DataSinkAthenaPluginConfig;

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_stdio_data_sink_plugin(
        "Athena",
        "Athena",
        false,
        "skippr-plugin-data-sink-athena",
        |_binding, buffer, env| async move {
            let cfg: DataSinkAthenaPluginConfig = env.decode().map_err(io::Error::other)?;
            Ok(DataSinkAthenaPlugin::new_with_config(buffer, cfg).await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-athena: {}", err);
        std::process::exit(1);
    }
}
