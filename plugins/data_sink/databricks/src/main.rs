use std::io;

use clap::Parser;
use skippr_plugin_data_sink_databricks::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct Cli {}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = Cli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "Databricks",
        "Databricks",
        skippr_runtime_sdk::plugins::cdc::sink_capabilities::by_name("Databricks").map(Into::into),
        false,
        "skippr-plugin-data-sink-databricks",
        |install| async move {
            let cfg: DataSinkDatabricksPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            Ok(DataSinkDatabricksPlugin::new_with_config(
                buffer_name_for_runtime_binding(install.binding),
                cfg,
            )
            .await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-databricks: {}", err);
        std::process::exit(1);
    }
});
