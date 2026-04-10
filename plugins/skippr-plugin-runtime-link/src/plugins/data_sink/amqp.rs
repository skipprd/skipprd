use crate::helpers::configuration::DataSinkPluginConfig;
use crate::plugins::DataSink;
use async_trait::async_trait;
use datafusion::execution::SendableRecordBatchStream;
use futures::StreamExt;
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

impl TryFrom<DataSinkPluginConfig> for DataSinkAmqpPluginConfig {
    type Error = String;

    fn try_from(entry: DataSinkPluginConfig) -> Result<Self, Self::Error> {
        entry.decode_for_plugin("Amqp")
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
        cdc_ctx: Option<&crate::plugins::cdc::SyncContext>,
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
        let mut row_offset: usize = 0;

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

                if let Some(ctx) = cdc_ctx {
                    if let Some(row_meta) = ctx.part_meta.rows.get(row_offset + row_idx) {
                        let mutation_str = match row_meta.mutation {
                            crate::plugins::cdc::MutationKind::Snapshot => "snapshot",
                            crate::plugins::cdc::MutationKind::Insert => "insert",
                            crate::plugins::cdc::MutationKind::Update => "update",
                            crate::plugins::cdc::MutationKind::Delete => "delete",
                        };
                        map.insert(
                            "_skippr_mutation".to_string(),
                            serde_json::Value::String(mutation_str.to_string()),
                        );
                        let token_hex: String = row_meta
                            .order_token
                            .iter()
                            .map(|b| format!("{:02x}", b))
                            .collect();
                        map.insert(
                            "_skippr_order_token".to_string(),
                            serde_json::Value::String(token_hex),
                        );
                    }
                }

                let json =
                    serde_json::to_vec(&map).map_err(|e| std::io::Error::other(e.to_string()))?;

                channel
                    .basic_publish(
                        &self.config.exchange,
                        routing_key,
                        BasicPublishOptions::default(),
                        &json,
                        BasicProperties::default().with_content_type("application/json".into()),
                    )
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                msg_count += 1;
            }
            row_offset += batch.num_rows();
        }

        info!(
            "AMQP: published {} messages to exchange '{}'",
            msg_count, self.config.exchange
        );
        counters::dec_uploads_in_flight();
        Ok(())
    }

    fn capability(&self) -> Option<&'static crate::plugins::cdc::SinkCapability> {
        Some(&crate::plugins::cdc::sink_capabilities::AMQP)
    }
}

impl DataSinkAmqpPlugin {
    pub async fn new_with_config(_buffer_name: String, config: DataSinkAmqpPluginConfig) -> Self {
        Self { config }
    }
}
