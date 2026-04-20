use std::io;

use clap::Parser;
use skippr_plugin_data_sink_bigquery::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_schema_sink_plugin,
};

#[derive(Debug, Parser)]
struct Cli {}

#[tokio::main]
async fn main() {
    let _cli = Cli::parse();
    if let Err(err) = run_runtime_schema_sink_plugin(
        "Bigquery",
        "Bigquery",
        "skippr-plugin-schema-sink-bigquery",
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
        eprintln!("skippr-plugin-schema-sink-bigquery: {}", err);
        std::process::exit(1);
    }
}
