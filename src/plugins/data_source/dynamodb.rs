use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use num_cpus;
use serde_derive::Deserialize;
use serde_json::{json, Map, Value};
use tracing::{error, info};

use crate::helpers::configuration::{Config, DataSourcePluginConfig};
use crate::helpers::offsets::{OffsetKey, OffsetTypes, Offsets};
use crate::ingest_work::{Ingest, IngestBatch, IngestTask, IngestTasks};
use crate::plugins::{DataSink, DataSource};

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceDynamodbPluginConfig {
    pub table_name: String,
    pub region: Option<String>,
    pub endpoint_url: Option<String>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
}

impl From<DataSourcePluginConfig> for DataSourceDynamodbPluginConfig {
    fn from(plugin_config: DataSourcePluginConfig) -> Self {
        match plugin_config {
            DataSourcePluginConfig::Dynamodb(config) => config,
            _ => panic!("Invalid plugin type for DynamoDB"),
        }
    }
}

pub struct DataSourceDynamodbPlugin {
    pub(crate) ingest: Ingest,
    pub(crate) config: DataSourceDynamodbPluginConfig,
    client: Client,
}

impl DataSourceDynamodbPlugin {
    pub async fn new() -> Self {
        let config: DataSourceDynamodbPluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(input_config) => input_config.into(),
                Err(_) => {
                    let table_name = Config::getenv("DYNAMODB_TABLE_NAME", "");
                    let region = {
                        let r = Config::getenv("AWS_DEFAULT_REGION", "");
                        if r.is_empty() {
                            None
                        } else {
                            Some(r)
                        }
                    };
                    DataSourceDynamodbPluginConfig {
                        table_name,
                        region,
                        endpoint_url: None,
                        format: Some("row".to_string()),
                        batch_size_bytes: None,
                        batch_size_seconds: None,
                    }
                }
            };

        let mut loader =
            aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(ref r) = config.region {
            loader = loader.region(aws_types::region::Region::new(r.clone()));
        }
        let shared_config = loader.load().await;
        let mut client_config = aws_sdk_dynamodb::config::Builder::from(&shared_config);
        if let Some(ref endpoint_url) = config.endpoint_url {
            client_config = client_config.endpoint_url(endpoint_url);
        }
        let client = Client::from_conf(client_config.build());

        DataSourceDynamodbPlugin {
            ingest: Ingest::new(),
            config,
            client,
        }
    }

    fn n_to_json(s: &str) -> Value {
        if let Ok(i) = s.parse::<i64>() {
            return json!(i);
        }
        if let Ok(u) = s.parse::<u64>() {
            return json!(u);
        }
        if let Ok(f) = s.parse::<f64>() {
            return json!(f);
        }
        Value::String(s.to_string())
    }

    fn attribute_to_json(av: &AttributeValue) -> Value {
        match av {
            AttributeValue::S(s) => Value::String(s.clone()),
            AttributeValue::N(s) => Self::n_to_json(s),
            AttributeValue::Bool(b) => Value::Bool(*b),
            AttributeValue::Null(true) | AttributeValue::Null(false) => Value::Null,
            AttributeValue::L(list) => {
                Value::Array(list.iter().map(Self::attribute_to_json).collect())
            }
            AttributeValue::M(map) => {
                let mut m = Map::new();
                for (k, v) in map {
                    m.insert(k.clone(), Self::attribute_to_json(v));
                }
                Value::Object(m)
            }
            AttributeValue::B(blob) => Value::String(B64.encode(blob.as_ref())),
            AttributeValue::Ss(ss) => Value::Array(ss.iter().cloned().map(Value::String).collect()),
            AttributeValue::Ns(ns) => Value::Array(ns.iter().map(|s| Self::n_to_json(s)).collect()),
            AttributeValue::Bs(bs) => Value::Array(
                bs.iter()
                    .map(|b| Value::String(B64.encode(b.as_ref())))
                    .collect(),
            ),
            _ => Value::Null,
        }
    }

    fn item_to_json(item: &HashMap<String, AttributeValue>) -> String {
        let mut map = Map::new();
        for (k, v) in item {
            map.insert(k.clone(), Self::attribute_to_json(v));
        }
        serde_json::to_string(&Value::Object(map)).unwrap_or_default()
    }

    pub async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        shared_output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) {
        info!("DynamoDB input plugin starting sync");

        let table_name = self.config.table_name.clone();
        if table_name.is_empty() {
            error!("DynamoDB table_name is empty");
            return;
        }

        let namespace = format!("dynamodb.{}", table_name);
        let total_segments = num_cpus::get().max(1) as i32;
        const DEFAULT_BATCH_ROWS: usize = 10_000;
        let batch_size = DEFAULT_BATCH_ROWS;

        let mut page: u64 = 0;

        for segment in 0..total_segments {
            let mut start_key: Option<HashMap<String, AttributeValue>> = None;
            loop {
                let offset_key = OffsetKey {
                    namespace: format!("dynamodb:{}", table_name),
                    partition: page.to_string(),
                };

                let mut scan = self
                    .client
                    .scan()
                    .table_name(&table_name)
                    .segment(segment)
                    .total_segments(total_segments);
                if let Some(ref key) = start_key {
                    scan = scan.set_exclusive_start_key(Some(key.clone()));
                }

                let out = match scan.send().await {
                    Ok(o) => o,
                    Err(e) => {
                        error!(
                            "DynamoDB scan failed (segment {}): {}",
                            segment, e
                        );
                        return;
                    }
                };

                let skip_ingest =
                    offsets.validate(&offset_key, OffsetTypes::Closed, 1) == Some(true);

                if !skip_ingest {
                    let items = out.items();
                    info!(
                        "DynamoDB segment {} page {}: {} items",
                        segment,
                        page,
                        items.len()
                    );

                    let mut current_batch: Vec<IngestBatch> = Vec::new();
                    let mut ingest_tasks = IngestTasks::new();
                    let mut wrote_tasks = false;

                    for item in items {
                        let json_str = Self::item_to_json(item);
                        let bytes = json_str.len();
                        current_batch.push(IngestBatch {
                            offset_key: offset_key.clone(),
                            data: json_str,
                            bytes,
                            source_uri: format!("dynamodb://{}", table_name),
                            namespace: Some(format!("dynamodb.{}", table_name)),
                        });

                        if current_batch.len() >= batch_size {
                            let batch = std::mem::take(&mut current_batch);
                            ingest_tasks.add(IngestTask::new(
                                batch,
                                offsets.clone(),
                                shared_output.clone(),
                            ));
                            wrote_tasks = true;
                        }
                    }

                    if !current_batch.is_empty() {
                        ingest_tasks.add(IngestTask::new(
                            current_batch,
                            offsets.clone(),
                            shared_output.clone(),
                        ));
                        wrote_tasks = true;
                    }

                    if wrote_tasks {
                        self.ingest.ingest_file(
                            &Arc::new(ingest_tasks),
                            &offsets,
                            shared_output.clone(),
                        );
                    }
                }

                page += 1;
                start_key = out.last_evaluated_key().cloned();
                if start_key.is_none() {
                    break;
                }
            }
        }

        info!("DynamoDB input plugin sync complete for {}", namespace);
    }
}

#[async_trait]
impl DataSource for DataSourceDynamodbPlugin {
    async fn sync(
        &mut self,
        offsets: Arc<Offsets>,
        output: Arc<Box<dyn DataSink + Send + Sync>>,
    ) -> Result<(), std::io::Error> {
        self.sync(offsets, output).await;
        Ok(())
    }
}
