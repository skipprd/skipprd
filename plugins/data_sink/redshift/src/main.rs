use std::io;

use clap::Parser;
use skippr::runtime_plugins::sink_stdio_entry::run_stdio_data_sink_plugin;
use skippr_plugin_runtime_link::runtime_sink_link::redshift::DataSinkRedshiftPlugin;
use skippr_plugin_runtime_link::runtime_sink_link::redshift::DataSinkRedshiftPluginConfig;

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_stdio_data_sink_plugin(
        "Redshift",
        "Redshift",
        true,
        "skippr-plugin-data-sink-redshift",
        |_binding, buffer, env| async move {
            let cfg: DataSinkRedshiftPluginConfig = env.decode().map_err(io::Error::other)?;
            Ok(DataSinkRedshiftPlugin::new_with_config(buffer, cfg).await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-redshift: {}", err);
        std::process::exit(1);
    }
}
