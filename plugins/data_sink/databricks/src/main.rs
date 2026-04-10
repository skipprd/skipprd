use std::io;

use clap::Parser;
use skippr::runtime_plugins::sink_stdio_entry::run_stdio_data_sink_plugin;
use skippr_plugin_runtime_link::runtime_sink_link::databricks::DataSinkDatabricksPlugin;
use skippr_plugin_runtime_link::runtime_sink_link::databricks::DataSinkDatabricksPluginConfig;

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_stdio_data_sink_plugin(
        "Databricks",
        "Databricks",
        false,
        "skippr-plugin-data-sink-databricks",
        |_binding, buffer, env| async move {
            let cfg: DataSinkDatabricksPluginConfig = env.decode().map_err(io::Error::other)?;
            Ok(DataSinkDatabricksPlugin::new_with_config(buffer, cfg).await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-databricks: {}", err);
        std::process::exit(1);
    }
}
