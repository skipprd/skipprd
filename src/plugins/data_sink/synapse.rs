use crate::helpers::configuration::DataSinkPluginConfig;
use crate::plugins::DataSink;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use futures_util::StreamExt;
use serde_derive::Deserialize;
use tiberius::{Client, Config as TibConfig};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tracing::info;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkSynapsePluginConfig {
    pub connection_string: String,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub format: Option<String>,
}

impl From<DataSinkPluginConfig> for DataSinkSynapsePluginConfig {
    fn from(plugin_config: DataSinkPluginConfig) -> Self {
        match plugin_config {
            DataSinkPluginConfig::Synapse(config) => config,
            _ => panic!("Invalid plugin type for Synapse"),
        }
    }
}

pub struct DataSinkSynapsePlugin {
    config: DataSinkSynapsePluginConfig,
}

#[async_trait]
impl DataSink for DataSinkSynapsePlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        _filename: String,
    ) -> Result<(), std::io::Error> {
        use crate::metrics::counters;
        counters::inc_uploads_in_flight();

        let tib_config = TibConfig::from_ado_string(&self.config.connection_string)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let tcp = TcpStream::connect(tib_config.get_addr())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        tcp.set_nodelay(true)?;
        let mut client = Client::connect(tib_config, tcp.compat_write())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let schema = self.config.schema.as_deref().unwrap_or("dbo");
        let table = self.config.table.as_deref().unwrap_or("data");

        let mut batch_stream = stream;
        while let Some(batch_result) = batch_stream.next().await {
            let batch = batch_result.map_err(|e| std::io::Error::other(e.to_string()))?;
            let schema_ref = batch.schema();

            for row_idx in 0..batch.num_rows() {
                let mut columns = Vec::new();
                let mut values = Vec::new();
                for (col_idx, field) in schema_ref.fields().iter().enumerate() {
                    let col = batch.column(col_idx);
                    let val = arrow::util::display::array_value_to_string(col, row_idx)
                        .unwrap_or_else(|_| "NULL".to_string());
                    columns.push(format!("[{}]", field.name()));
                    if val == "NULL" || val.is_empty() {
                        values.push("NULL".to_string());
                    } else {
                        values.push(format!("N'{}'", val.replace('\'', "''")));
                    }
                }

                let sql = format!(
                    "INSERT INTO [{}].[{}] ({}) VALUES ({})",
                    schema,
                    table,
                    columns.join(", "),
                    values.join(", ")
                );
                client
                    .execute(&sql, &[])
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
            }
        }

        info!("Synapse: inserted rows into [{}].[{}]", schema, table);
        counters::dec_uploads_in_flight();
        Ok(())
    }
}

impl DataSinkSynapsePlugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: DataSinkSynapsePluginConfig,
    ) -> Self {
        Self { config }
    }
}
