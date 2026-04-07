use async_trait::async_trait;
use datafusion::arrow::json::writer::LineDelimited;
use datafusion::arrow::json::WriterBuilder;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;

use crate::plugins::DataSink;

pub struct DataSinkStdoutPlugin;

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
        cdc_ctx: Option<&crate::plugins::cdc::SyncContext>,
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

    fn capability(&self) -> Option<&'static crate::plugins::cdc::SinkCapability> {
        Some(&crate::plugins::cdc::sink_capabilities::STDOUT)
    }
}
