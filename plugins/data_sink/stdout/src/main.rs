use clap::Parser;
use skippr_plugin_data_sink_stdout::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct StdoutSinkRuntimePluginCli {}

#[tokio::main]
async fn main() {
    let _cli = StdoutSinkRuntimePluginCli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "Stdout",
        "Stdout",
        skippr_core::plugins::cdc::sink_capabilities::by_name("Stdout").map(Into::into),
        false,
        "skippr-plugin-data-sink-stdout",
        |install| async move {
            Ok(DataSinkStdoutPlugin::new(buffer_name_for_runtime_binding(install.binding)).await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-stdout: {}", err);
        std::process::exit(1);
    }
}
