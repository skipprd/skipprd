use std::io;

use clap::Parser;
use skippr_plugin_data_sink_snowflake::*;
use skippr_runtime_sdk::runtime_main::run_runtime_main;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct Cli {}

fn main() {
    run_runtime_main("snowflake-data-sink-main", async {
        let _cli = Cli::parse();
        if let Err(err) = run_runtime_data_sink_plugin(
            "Snowflake",
            "Snowflake",
            skippr_core::plugins::cdc::sink_capabilities::by_name("Snowflake").map(Into::into),
            true,
            "skippr-plugin-data-sink-snowflake",
            |install| async move {
                let cfg: DataSinkSnowflakePluginConfig =
                    install.config.0.decode().map_err(io::Error::other)?;
                Ok(DataSinkSnowflakePlugin::new_with_config(
                    buffer_name_for_runtime_binding(install.binding),
                    cfg,
                )
                .await)
            },
        )
        .await
        {
            eprintln!("skippr-plugin-data-sink-snowflake: {}", err);
            std::process::exit(1);
        }
    });
}
