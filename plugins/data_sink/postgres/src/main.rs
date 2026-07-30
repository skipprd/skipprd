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

    async fn sync_grouped(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<skippr_runtime_sdk::plugins::SinkWriteOutcome, io::Error> {
        let schema = reader.schema();
        while let Some(chunk) = reader.next_chunk().await? {
            let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
            let chunk_ctx = ctx.chunk_sink_write_context_with_cdc(
                chunk.chunk_index,
                chunk.chunk_index == 0 && chunk.final_chunk,
                chunk_cdc.as_ref(),
            );
            self.sync(
                chunk.into_stream(schema.clone()),
                chunk_ctx.filename,
                chunk_ctx.cdc_ctx,
            )
            .await?;
        }
        Ok(skippr_runtime_sdk::plugins::SinkWriteOutcome::Applied)
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::POSTGRES
    }
}

skippr_runtime_sdk::runtime_main!(async {
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
});
