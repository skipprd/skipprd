use std::io;

use clap::Parser;
use skippr::runtime_plugins::sink_stdio_entry::run_stdio_data_sink_plugin;
use skippr_plugin_runtime_link::runtime_sink_link::synapse::DataSinkSynapsePlugin;
use skippr_plugin_runtime_link::runtime_sink_link::synapse::DataSinkSynapsePluginConfig;

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_stdio_data_sink_plugin(
        "Synapse",
        "Synapse",
        false,
        "skippr-plugin-data-sink-synapse",
        |_binding, buffer, env| async move {
            let cfg: DataSinkSynapsePluginConfig = env.decode().map_err(io::Error::other)?;
            Ok(DataSinkSynapsePlugin::new_with_config(buffer, cfg).await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-synapse: {}", err);
        std::process::exit(1);
    }
}
