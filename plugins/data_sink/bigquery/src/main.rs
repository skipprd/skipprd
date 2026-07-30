use std::io;

use clap::Parser;
use skippr_plugin_data_sink_bigquery::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct Cli {}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = Cli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "Bigquery",
        "Bigquery",
        skippr_runtime_sdk::plugins::cdc::sink_capabilities::by_name("Bigquery").map(Into::into),
        true,
        "skippr-plugin-data-sink-bigquery",
        |install| async move {
            let cfg: DataSinkBigqueryPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            Ok(DataSinkBigqueryPlugin::new_with_config(
                buffer_name_for_runtime_binding(install.binding),
                cfg,
            )
            .await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-bigquery: {}", err);
        std::process::exit(1);
    }
});
