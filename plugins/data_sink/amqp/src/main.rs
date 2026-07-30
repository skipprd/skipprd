use std::io;

use clap::Parser;
use skippr_plugin_data_sink_amqp::*;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct AmqpSinkRuntimePluginCli {}

skippr_runtime_sdk::runtime_main!(async {
    let _cli = AmqpSinkRuntimePluginCli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "Amqp",
        "Amqp",
        skippr_runtime_sdk::plugins::cdc::sink_capabilities::by_name("Amqp").map(Into::into),
        false,
        "skippr-plugin-data-sink-amqp",
        |install| async move {
            let config: DataSinkAmqpPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            Ok(DataSinkAmqpPlugin::new_with_config(
                buffer_name_for_runtime_binding(install.binding),
                config,
            )
            .await)
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-amqp: {}", err);
        std::process::exit(1);
    }
});
