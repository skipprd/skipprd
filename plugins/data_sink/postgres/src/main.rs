use std::io;

use clap::Parser;
use datafusion::execution::SendableRecordBatchStream;
use skippr_plugin_data_sink_postgres::{DataSinkPostgresPlugin, DataSinkPostgresPluginConfig};
use skippr_runtime_sdk::plugins::cdc::SyncContext;
use skippr_runtime_sdk::plugins::DataSink;
use skippr_runtime_sdk::sink_runtime_entry::{
    buffer_name_for_runtime_binding, run_runtime_data_sink_plugin,
};

#[derive(Debug, Parser)]
struct PostgresSinkRuntimePluginCli {}

struct PostgresSinkRuntimePlugin {
    inner: DataSinkPostgresPlugin,
}

skippr_runtime_sdk::declare_sink_spec!(
    PostgresSinkSpec,
    PostgresSinkRuntimePlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::POSTGRES,
    skippr_runtime_sdk::plugins::FinalStateIdempotentApply
);

#[async_trait::async_trait]
impl DataSink for PostgresSinkRuntimePlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        filename: String,
        cdc_ctx: Option<&SyncContext>,
    ) -> Result<(), io::Error> {
        self.inner.sync(stream, filename, cdc_ctx).await
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::POSTGRES
    }
}

#[tokio::main]
async fn main() {
    let _cli = PostgresSinkRuntimePluginCli::parse();
    if let Err(err) = run_runtime_data_sink_plugin(
        "Postgres",
        "Postgres",
        skippr_runtime_sdk::plugins::cdc::sink_capabilities::by_name("Postgres").map(Into::into),
        true,
        "skippr-plugin-data-sink-postgres",
        |install| async move {
            let config: DataSinkPostgresPluginConfig =
                install.config.0.decode().map_err(io::Error::other)?;
            Ok(PostgresSinkRuntimePlugin {
                inner: DataSinkPostgresPlugin::new_with_config(
                    buffer_name_for_runtime_binding(install.binding),
                    config,
                )
                .await,
            })
        },
    )
    .await
    {
        eprintln!("skippr-plugin-data-sink-postgres: {}", err);
        std::process::exit(1);
    }
}
