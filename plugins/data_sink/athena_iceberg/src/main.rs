use std::io;

use clap::Parser;
use skippr_plugin_data_sink_athena_iceberg::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct AthenaIcebergSinkRuntimePluginCli {}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = AthenaIcebergSinkRuntimePluginCli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        AthenaIcebergConfig::PLUGIN_NAME,
        AthenaIcebergConfig::PLUGIN_NAME,
        skippr_runtime_sdk::plugins::cdc::sink_capabilities::by_name(
            AthenaIcebergConfig::PLUGIN_NAME,
        )
        .map(Into::into),
        true,
        "skippr-plugin-data-sink-athena-iceberg",
        |install| async move {
            let cfg: AthenaIcebergConfig = install.config.0.decode().map_err(io::Error::other)?;
            open_writer(
                &cfg,
                install.context,
                install.binding,
                buffer_name_for_runtime_binding(install.binding),
            )
            .await
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-athena-iceberg: {}", err);
        std::process::exit(1);
    }
});
