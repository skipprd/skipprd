use std::io;

use clap::Parser;
use skippr_plugin_data_sink_sftp::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct SftpSinkRuntimePluginCli {}

#[tokio::main]
async fn main() {
    let _cli = SftpSinkRuntimePluginCli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "Sftp",
        "Sftp",
        skippr_core::plugins::cdc::sink_capabilities::by_name("Sftp").map(Into::into),
        false,
        "skippr-plugin-data-sink-sftp",
        |install| async move {
            let config: DataSinkSftpPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            Ok(DataSinkSftpPlugin::new_with_config(
                buffer_name_for_runtime_binding(install.binding),
                config,
            )
            .await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-sftp: {}", err);
        std::process::exit(1);
    }
}
