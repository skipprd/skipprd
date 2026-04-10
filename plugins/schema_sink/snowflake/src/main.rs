use std::io;

use clap::Parser;
use skippr::helpers::configuration::DataSinkSnowflakePluginConfig;
use skippr::runtime_plugins::sink_stdio_entry::run_stdio_schema_sink_plugin;
use skippr_plugin_runtime_link::runtime_sink_link::snowflake::DataSinkSnowflakePlugin;

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_stdio_schema_sink_plugin(
        "Snowflake",
        "Snowflake",
        "skippr-plugin-schema-sink-snowflake",
        |_binding, buffer, env| async move {
            let cfg: DataSinkSnowflakePluginConfig = env.decode().map_err(io::Error::other)?;
            Ok(DataSinkSnowflakePlugin::new_with_config(buffer, cfg).await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-schema-sink-snowflake: {}", err);
        std::process::exit(1);
    }
}
