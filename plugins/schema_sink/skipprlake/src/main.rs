use std::io;

use clap::Parser;
use skippr_plugin_data_sink_skipprlake::*;
use skippr_runtime_sdk::sink_runtime_entry::run_runtime_schema_sink_plugin;

#[derive(Debug, Parser)]
struct SkipprLakeSchemaRuntimePluginCli {}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = SkipprLakeSchemaRuntimePluginCli::parse();
    if let Err(err) = run_runtime_schema_sink_plugin(
        SkipprLakeConfig::PLUGIN_NAME,
        SkipprLakeConfig::PLUGIN_NAME,
        "skippr-plugin-schema-sink-skipprlake",
        |install| async move {
            let cfg: SkipprLakeConfig = install.config.0.decode().map_err(io::Error::other)?;
            open_writer(&cfg, install.context, install.binding, "schema".to_string()).await
        },
    )
    .await
    {
        eprintln!("skippr-plugin-schema-sink-skipprlake: {}", err);
        std::process::exit(1);
    }
});
