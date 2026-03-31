use crate::helpers::configuration::DataSinkPluginConfig;
use crate::plugins::DataSink;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use futures_util::StreamExt;
use lapin::{
    options::*, types::FieldTable, BasicProperties, Connection, ConnectionProperties, ExchangeKind,
};
use serde_derive::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize, Clone)]
pub struct DataSinkAmqpPluginConfig {
    pub connection_string: String,
    pub exchange: String,
    pub routing_key: Option<String>,
    pub exchange_type: Option<String>,
    pub format: Option<String>,
}

impl From<DataSinkPluginConfig> for DataSinkAmqpPluginConfig {
    fn from(plugin_config: DataSinkPluginConfig) -> Self {
        match plugin_config {
            DataSinkPluginConfig::Amqp(config) => config,
            _ => panic!("Invalid plugin type for AMQP output"),
        }
    }
}

pub struct DataSinkAmqpPlugin {
    config: DataSinkAmqpPluginConfig,
}

#[async_trait]
impl DataSink for DataSinkAmqpPlugin {
    async fn sync(
        &self,
        stream: SendableRecordBatchStream,
        _filename: String,
    ) -> Result<(), std::io::Error> {
        use crate::metrics::counters;
        counters::inc_uploads_in_flight();

        let conn = Connection::connect(
            &self.config.connection_string,
            ConnectionProperties::default(),
        )
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        let channel = conn
            .create_channel()
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let exchange_kind = match self.config.exchange_type.as_deref() {
            Some("fanout") => ExchangeKind::Fanout,
            Some("topic") => ExchangeKind::Topic,
            Some("headers") => ExchangeKind::Headers,
            _ => ExchangeKind::Direct,
        };

        channel
            .exchange_declare(
                &self.config.exchange,
                exchange_kind,
                ExchangeDeclareOptions {
                    durable: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        let routing_key = self.config.routing_key.as_deref().unwrap_or("");
        let mut msg_count: u64 = 0;

        let mut batch_stream = stream;
        while let Some(batch_result) = batch_stream.next().await {
            let batch = batch_result.map_err(|e| std::io::Error::other(e.to_string()))?;
            let schema = batch.schema();

            for row_idx in 0..batch.num_rows() {
                let mut map = serde_json::Map::new();
                for (col_idx, field) in schema.fields().iter().enumerate() {
                    let col = batch.column(col_idx);
                    let val = arrow::util::display::array_value_to_string(col, row_idx)
                        .unwrap_or_else(|_| "null".to_string());
                    map.insert(field.name().clone(), serde_json::Value::String(val));
                }
                let json = serde_json::to_vec(&map)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;

                channel
                    .basic_publish(
                        &self.config.exchange,
                        routing_key,
                        BasicPublishOptions::default(),
                        &json,
                        BasicProperties::default()
                            .with_content_type("application/json".into()),
                    )
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                msg_count += 1;
            }
        }

        info!(
            "AMQP: published {} messages to exchange '{}'",
            msg_count, self.config.exchange
        );
        counters::dec_uploads_in_flight();
        Ok(())
    }
}

impl DataSinkAmqpPlugin {
    pub async fn new_with_config(
        _buffer_name: String,
        config: DataSinkAmqpPluginConfig,
    ) -> Self {
        Self { config }
    }
}
