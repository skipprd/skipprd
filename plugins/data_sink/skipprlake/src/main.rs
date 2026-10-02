use std::io;

use clap::Parser;
use skippr_plugin_data_sink_skipprlake::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct SkipprLakeSinkRuntimePluginCli {}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = SkipprLakeSinkRuntimePluginCli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        SkipprLakeConfig::PLUGIN_NAME,
        SkipprLakeConfig::PLUGIN_NAME,
        skippr_runtime_sdk::plugins::cdc::sink_capabilities::by_name(SkipprLakeConfig::PLUGIN_NAME)
            .map(Into::into),
        true,
        "skippr-plugin-data-sink-skipprlake",
        |install| async move {
            let cfg: SkipprLakeConfig = install.config.0.decode().map_err(io::Error::other)?;
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
        eprintln!("skippr-plugin-data-sink-skipprlake: {}", err);
        std::process::exit(1);
    }
});
