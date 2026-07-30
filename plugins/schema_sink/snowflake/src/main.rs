use std::io;

use clap::Parser;
use skippr_plugin_data_sink_snowflake::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_schema_sink_plugin,
};

#[derive(Debug, Parser)]
struct Cli {}

skippr_runtime_sdk::runtime_main!("snowflake-schema-sink-main", async {
    let _cli = Cli::parse();
    if let Err(err) = run_runtime_schema_sink_plugin(
        "Snowflake",
        "Snowflake",
        "skippr-plugin-schema-sink-snowflake",
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
        eprintln!("skippr-plugin-schema-sink-snowflake: {}", err);
        std::process::exit(1);
    }
});
