use async_trait::async_trait;
use datafusion::arrow::json::writer::LineDelimited;
use datafusion::arrow::json::WriterBuilder;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;

use skippr_runtime_sdk::plugins::DataSink;

pub struct DataSinkStdoutPlugin;

skippr_runtime_sdk::declare_sink_spec!(
    StdoutSinkSpec,
    DataSinkStdoutPlugin,
    skippr_runtime_sdk::plugins::cdc::sink_capabilities::STDOUT,
    skippr_runtime_sdk::plugins::AtLeastOnceMessageDelivery
);

impl DataSinkStdoutPlugin {
    pub async fn new(_buffer_name: String) -> Self {
        Self
    }
}

#[async_trait]
impl DataSink for DataSinkStdoutPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        _filename: String,
        cdc_ctx: Option<&skippr_runtime_sdk::plugins::cdc::SyncContext>,
    ) -> Result<(), std::io::Error> {
        let mut stream = match cdc_ctx {
            Some(ctx) => super::cdc_encode::augment_stream_with_cdc_columns(stream, &ctx.part_meta),
            None => stream,
        };
        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut buf = Vec::new();
            let mut writer = WriterBuilder::new().build::<_, LineDelimited>(&mut buf);
            writer
                .write(&batch)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            writer
                .finish()
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let output = String::from_utf8_lossy(&buf);
            print!("{}", output);
        }
        Ok(())
    }

    async fn sync_with_context(
        &self,
        stream: SendableRecordBatchStream,
        ctx: skippr_runtime_sdk::plugins::SinkWriteContext<'_>,
    ) -> Result<(), std::io::Error> {
        use skippr_runtime_sdk::plugins::AtLeastOnceMessageDelivery;
        ctx.validate_grouped::<AtLeastOnceMessageDelivery>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Unsupported, e))?;
        self.sync(stream, ctx.filename, ctx.cdc_ctx).await
    }

    async fn sync_grouped(
        &self,
        mut reader: skippr_runtime_sdk::plugins::GroupedBatchReader,
        ctx: skippr_runtime_sdk::plugins::GroupedSinkWriteContext<'_>,
    ) -> Result<skippr_runtime_sdk::plugins::SinkWriteOutcome, std::io::Error> {
        let schema = reader.schema();
        while let Some(chunk) = reader.next_chunk().await? {
            let chunk_cdc = ctx.chunk_cdc_context(&chunk)?;
            let chunk_ctx = ctx.chunk_sink_write_context_with_cdc(
                chunk.chunk_index,
                chunk.chunk_index == 0 && chunk.final_chunk,
                chunk_cdc.as_ref(),
            );
            self.sync_with_context(chunk.into_stream(schema.clone()), chunk_ctx)
                .await?;
        }
        Ok(skippr_runtime_sdk::plugins::SinkWriteOutcome::Applied)
    }

    fn capability(&self) -> &'static skippr_runtime_sdk::plugins::cdc::SinkCapability {
        &skippr_runtime_sdk::plugins::cdc::sink_capabilities::STDOUT
    }
}
