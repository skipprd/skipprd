use std::io;

use clap::Parser;
use skippr_plugin_data_sink_s3::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct S3SinkRuntimePluginCli {}

#[tokio::main]
async fn main() {
    let _cli = S3SinkRuntimePluginCli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "S3",
        "S3",
        skippr_runtime_sdk::plugins::cdc::sink_capabilities::by_name("S3").map(Into::into),
        false,
        "skippr-plugin-data-sink-s3",
        |install| async move {
            let config: DataSinkS3PluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            DataSinkS3Plugin::new_with_config(
                buffer_name_for_runtime_binding(install.binding),
                config,
            )
            .await
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-s3: {}", err);
        std::process::exit(1);
    }
}
