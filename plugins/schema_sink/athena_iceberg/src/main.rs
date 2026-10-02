use std::io;

use clap::Parser;
use skippr_plugin_data_sink_athena_iceberg::*;
use skippr_runtime_sdk::sink_runtime_entry::run_runtime_schema_sink_plugin;

#[derive(Debug, Parser)]
struct AthenaIcebergSchemaRuntimePluginCli {}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = AthenaIcebergSchemaRuntimePluginCli::parse();
    if let Err(err) = run_runtime_schema_sink_plugin(
        AthenaIcebergConfig::PLUGIN_NAME,
        AthenaIcebergConfig::PLUGIN_NAME,
        "skippr-plugin-schema-sink-athena-iceberg",
        |install| async move {
            let cfg: AthenaIcebergConfig = install.config.0.decode().map_err(io::Error::other)?;
            open_writer(&cfg, install.context, install.binding, "schema".to_string()).await
        },
    )
    .await
    {
        eprintln!("skippr-plugin-schema-sink-athena-iceberg: {}", err);
        std::process::exit(1);
    }
});
