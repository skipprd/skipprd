use std::io;

use clap::Parser;
use skippr_plugin_data_sink_iceberg::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct IcebergSinkRuntimePluginCli {}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = IcebergSinkRuntimePluginCli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "Iceberg",
        "Iceberg",
        skippr_runtime_sdk::plugins::cdc::sink_capabilities::by_name("Iceberg").map(Into::into),
        true,
        "skippr-plugin-data-sink-iceberg",
        |install| async move {
            let config: DataSinkIcebergPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            DataSinkIcebergPlugin::new_with_config(
                install.context,
                install.binding,
                buffer_name_for_runtime_binding(install.binding),
                config,
            )
            .await
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-iceberg: {}", err);
        std::process::exit(1);
    }
});
