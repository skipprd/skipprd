use std::io;

use clap::Parser;
use skippr_plugin_data_sink_clickhouse::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_schema_sink_plugin,
};

#[derive(Debug, Parser)]
struct Cli {}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = Cli::parse();
    if let Err(err) = run_runtime_schema_sink_plugin(
        "Clickhouse",
        "Clickhouse",
        "skippr-plugin-schema-sink-clickhouse",
        |install| async move {
            let cfg: DataSinkClickhousePluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            Ok(DataSinkClickhousePlugin::new_with_config(
                buffer_name_for_runtime_binding(install.binding),
                cfg,
            )
            .await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-schema-sink-clickhouse: {}", err);
        std::process::exit(1);
    }
});
