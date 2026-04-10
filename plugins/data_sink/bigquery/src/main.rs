use std::io;

use clap::Parser;
use skippr::helpers::configuration::DataSinkBigqueryPluginConfig;
use skippr::runtime_plugins::sink_stdio_entry::run_stdio_data_sink_plugin;
use skippr_plugin_runtime_link::runtime_sink_link::bigquery::DataSinkBigqueryPlugin;

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_stdio_data_sink_plugin(
        "Bigquery",
        "Bigquery",
        true,
        "skippr-plugin-data-sink-bigquery",
        |_binding, buffer, env| async move {
            let cfg: DataSinkBigqueryPluginConfig = env.decode().map_err(io::Error::other)?;
            Ok(DataSinkBigqueryPlugin::new_with_config(buffer, cfg).await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-bigquery: {}", err);
        std::process::exit(1);
    }
}
