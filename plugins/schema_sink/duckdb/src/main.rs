use std::io;

use clap::Parser;
use skippr_plugin_data_sink_duckdb::*;
use skippr_runtime_sdk::sink_runtime_entry::run_runtime_schema_sink_plugin;

#[derive(Debug, Parser)]
struct DuckdbSchemaRuntimePluginCli {}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = DuckdbSchemaRuntimePluginCli::parse();
    if let Err(err) = run_runtime_schema_sink_plugin(
        DuckdbConfig::PLUGIN_NAME,
        DuckdbConfig::PLUGIN_NAME,
        "skippr-plugin-schema-sink-duckdb",
        |install| async move {
            let cfg: DuckdbConfig = install.config.0.decode().map_err(io::Error::other)?;
            open_writer(&cfg, install.context, install.binding, "schema".to_string()).await
        },
    )
    .await
    {
        eprintln!("skippr-plugin-schema-sink-duckdb: {}", err);
        std::process::exit(1);
    }
});
