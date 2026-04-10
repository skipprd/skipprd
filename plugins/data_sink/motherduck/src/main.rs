use std::io;

use clap::Parser;
use skippr::runtime_plugins::sink_stdio_entry::run_stdio_data_sink_plugin;
use skippr_plugin_runtime_link::runtime_sink_link::motherduck::DataSinkMotherduckPlugin;
use skippr_plugin_runtime_link::runtime_sink_link::motherduck::DataSinkMotherduckPluginConfig;

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_stdio_data_sink_plugin(
        "Motherduck",
        "Motherduck",
        true,
        "skippr-plugin-data-sink-motherduck",
        |_binding, buffer, env| async move {
            let cfg: DataSinkMotherduckPluginConfig = env.decode().map_err(io::Error::other)?;
            Ok(DataSinkMotherduckPlugin::new_with_config(buffer, cfg).await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-motherduck: {}", err);
        std::process::exit(1);
    }
}
