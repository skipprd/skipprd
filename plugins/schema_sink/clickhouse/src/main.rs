use std::io;

use clap::Parser;
use skippr::runtime_plugins::sink_stdio_entry::run_stdio_schema_sink_plugin;
use skippr_plugin_runtime_link::runtime_sink_link::clickhouse::{
    DataSinkClickhousePlugin, DataSinkClickhousePluginConfig,
};

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_stdio_schema_sink_plugin(
        "Clickhouse",
        "Clickhouse",
        "skippr-plugin-schema-sink-clickhouse",
        |_binding, buffer, env| async move {
            let cfg: DataSinkClickhousePluginConfig = env.decode().map_err(io::Error::other)?;
            Ok(DataSinkClickhousePlugin::new_with_config(buffer, cfg).await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-schema-sink-clickhouse: {}", err);
        std::process::exit(1);
    }
}
