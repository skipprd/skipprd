use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodbstreams::types::{OperationType, ShardIteratorType};
use aws_sdk_dynamodbstreams::Client as StreamsClient;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use num_cpus;
use serde_derive::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tracing::{error, info, warn};

use crate::helpers::configuration::Config;
use crate::helpers::plugin_config::PluginConfigEntry;
use skippr_runtime_sdk::plugins::cdc::{
    source_capabilities, DynamodbCheckpoint, MutationKind, WalRowMeta,
};
use skippr_runtime_sdk::plugins::{
    DataSource, SourceCdcMode, SourceExecutionContract, SourceOnceContract,
};
use skippr_runtime_sdk::progress::OffsetKey;
use skippr_runtime_sdk::source_compat::{
    load_checkpoint_payload, partition_already_closed, store_checkpoint_payload,
    submit_payload_batch_groups, submit_payload_batches, IngestBatch, SourceSyncContext,
};

/// CDC scan configuration passed to `sync_scan` when CDC tagging is needed.
struct CdcScanConfig {
    key_attrs: Vec<String>,
    anchor_bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DynamodbSnapshotCheckpoint {
    stream_arn: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DataSourceDynamodbPluginConfig {
    pub table_name: String,
    pub region: Option<String>,
    pub endpoint_url: Option<String>,
    pub format: Option<String>,
    pub batch_size_bytes: Option<i64>,
    pub batch_size_seconds: Option<i64>,
    #[serde(default)]
    pub cdc_mode: SourceCdcMode,
}

impl TryFrom<PluginConfigEntry> for DataSourceDynamodbPluginConfig {
    type Error = String;

    fn try_from(plugin_config: PluginConfigEntry) -> Result<Self, Self::Error> {
        plugin_config.decode_for_plugin("Dynamodb")
    }
}

pub struct DataSourceDynamodbPlugin {
    pub(crate) config: DataSourceDynamodbPluginConfig,
    client: Client,
    streams_client: StreamsClient,
}

impl DataSourceDynamodbPlugin {
    async fn from_config(config: DataSourceDynamodbPluginConfig) -> Self {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(ref r) = config.region {
            loader = loader.region(aws_types::region::Region::new(r.clone()));
        }
        let shared_config = loader.load().await;
        let mut client_config = aws_sdk_dynamodb::config::Builder::from(&shared_config);
        if let Some(ref endpoint_url) = config.endpoint_url {
            client_config = client_config.endpoint_url(endpoint_url);
        }
        let sdk_conf = client_config.build();
        let client = Client::from_conf(sdk_conf.clone());

        let mut streams_config = aws_sdk_dynamodbstreams::config::Builder::from(&shared_config);
        if let Some(ref endpoint_url) = config.endpoint_url {
            streams_config = streams_config.endpoint_url(endpoint_url);
        }
        let streams_client = StreamsClient::from_conf(streams_config.build());

        DataSourceDynamodbPlugin {
            config,
            client,
            streams_client,
        }
    }

    pub async fn new() -> Self {
        let config: DataSourceDynamodbPluginConfig =
            match Config::get_pipeline_input_plugin_config() {
                Ok(input_config) => input_config.try_into().unwrap_or_else(|e| panic!("{}", e)),
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
                        cdc_mode: SourceCdcMode::Snapshot,
                    }
                }
            };

        Self::from_config(config).await
    }

    pub async fn with_runtime_config(config: DataSourceDynamodbPluginConfig) -> Self {
        Self::from_config(config).await
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

    fn cdc_mode(&self) -> SourceCdcMode {
        self.config.cdc_mode
    }

    fn item_key_hash(item: &HashMap<String, AttributeValue>, key_attrs: &[String]) -> Vec<u8> {
        let mut key_str = String::new();
        for attr in key_attrs {
            if let Some(val) = item.get(attr) {
                key_str.push_str(&format!("{}={};", attr, Self::attribute_to_json(val)));
            }
        }
        md5::compute(key_str.as_bytes()).0.to_vec()
    }

    async fn sync_scan(
        &mut self,
        ctx: Arc<dyn SourceSyncContext>,
        cdc_config: Option<&CdcScanConfig>,
    ) -> std::io::Result<()> {
        info!("DynamoDB input plugin starting sync");

        let table_name = self.config.table_name.clone();
        if table_name.is_empty() {
            error!("DynamoDB table_name is empty");
            return Ok(());
        }

        let namespace = format!("dynamodb.{}", table_name);
        let total_segments = num_cpus::get().max(1) as i32;
        const DEFAULT_BATCH_ROWS: usize = 10_000;
        let batch_size = DEFAULT_BATCH_ROWS;

        let mut page: u64 = 0;
        let mut global_row_idx: u64 = 0;

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
                        error!("DynamoDB scan failed (segment {}): {}", segment, e);
                        return Ok(());
                    }
                };

                let skip_ingest = partition_already_closed(ctx.as_ref(), &offset_key);

                if !skip_ingest {
                    let items = out.items();
                    info!(
                        "DynamoDB segment {} page {}: {} items",
                        segment,
                        page,
                        items.len()
                    );

                    let mut current_batch: Vec<IngestBatch> = Vec::new();
                    let mut batch_groups: Vec<Vec<IngestBatch>> = Vec::new();

                    for item in items {
                        let json_str = Self::item_to_json(item);
                        let bytes = json_str.len();

                        let cdc_rows = if let Some(cfg) = cdc_config {
                            let event_id = if cfg.key_attrs.is_empty() {
                                format!("{}:{}:{}", table_name, segment, global_row_idx)
                                    .into_bytes()
                            } else {
                                Self::item_key_hash(item, &cfg.key_attrs)
                            };
                            global_row_idx += 1;
                            Some(vec![WalRowMeta {
                                mutation: MutationKind::Snapshot,
                                event_id,
                                order_token: cfg.anchor_bytes.clone(),
                            }])
                        } else {
                            None
                        };

                        current_batch.push(IngestBatch {
                            offset_key: offset_key.clone(),
                            data: json_str,
                            bytes,
                            offset_pos: None,
                            source_uri: format!("dynamodb://{}", table_name),
                            namespace: Some(format!("dynamodb.{}", table_name)),
                            cdc_rows,
                        });

                        if current_batch.len() >= batch_size {
                            batch_groups.push(std::mem::take(&mut current_batch));
                        }
                    }

                    if !current_batch.is_empty() {
                        batch_groups.push(current_batch);
                    }

                    if !batch_groups.is_empty() {
                        submit_payload_batch_groups(ctx.as_ref(), batch_groups)?;
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
        Ok(())
    }

    // -----------------------------------------------------------------------
    // CDC: anchored snapshot + DynamoDB Streams consumption
    // -----------------------------------------------------------------------

    async fn sync_cdc(
        &mut self,
        ctx: Arc<dyn SourceSyncContext>,
        mode: SourceCdcMode,
    ) -> Result<(), std::io::Error> {
        let table_name = self.config.table_name.clone();

        let key_attrs: Vec<String> = match self
            .client
            .describe_table()
            .table_name(&table_name)
            .send()
            .await
        {
            Ok(resp) => resp
                .table()
                .map(|t| {
                    t.key_schema()
                        .iter()
                        .map(|k| k.attribute_name().to_string())
                        .collect()
                })
                .unwrap_or_default(),
            Err(e) => {
                info!(
                    "DynamoDB CDC: describe_table failed ({}), using index-based event_ids",
                    e
                );
                Vec::new()
            }
        };

        // --- Phase 1: discover the stream ARN ---
        let stream_arn = self.discover_stream_arn(&table_name).await?;
        let snapshot_checkpoint_key = format!("dynamodb:{}:snapshot_complete", table_name);
        let snapshot_done = load_checkpoint_payload::<DynamodbSnapshotCheckpoint>(
            ctx.as_ref(),
            &snapshot_checkpoint_key,
        )
        .is_some();
        let should_run_snapshot = mode.includes_initial_snapshot() && !snapshot_done;

        // --- Phase 2: anchored snapshot ---
        let anchor_bytes = chrono::Utc::now().timestamp_millis().to_be_bytes().to_vec();

        if should_run_snapshot {
            info!(
                "DynamoDB CDC: anchored snapshot for {} (key_attrs={:?})",
                &table_name, key_attrs
            );

            let cfg = CdcScanConfig {
                key_attrs: key_attrs.clone(),
                anchor_bytes,
            };
            self.sync_scan(ctx.clone(), Some(&cfg)).await?;
            store_checkpoint_payload(
                ctx.as_ref(),
                &snapshot_checkpoint_key,
                &DynamodbSnapshotCheckpoint {
                    stream_arn: stream_arn.clone(),
                },
            )?;
        } else if snapshot_done {
            info!("DynamoDB CDC: skipping snapshot (bootstrap checkpoint exists)");
        } else if mode == SourceCdcMode::CdcOnly {
            info!("DynamoDB CDC: cdc_only mode skips initial snapshot");
        }

        // --- Phase 3: consume DynamoDB Streams ---
        info!(
            "DynamoDB CDC: starting stream consumption from {}",
            stream_arn
        );
        self.consume_stream(&stream_arn, &table_name, &key_attrs, ctx, mode)
            .await
    }

    async fn discover_stream_arn(&self, table_name: &str) -> Result<String, std::io::Error> {
        let resp = self
            .streams_client
            .list_streams()
            .table_name(table_name)
            .send()
            .await
            .map_err(|e| {
                std::io::Error::other(format!("DynamoDB Streams list_streams failed: {}", e))
            })?;

        let streams = resp.streams();
        let stream = streams.first().ok_or_else(|| {
            std::io::Error::other(format!(
                "No DynamoDB Stream found for table '{}'. Enable streams on the table with StreamViewType=NEW_AND_OLD_IMAGES.",
                table_name
            ))
        })?;

        stream
            .stream_arn()
            .map(|s| s.to_string())
            .ok_or_else(|| std::io::Error::other("DynamoDB Stream has no ARN"))
    }

    async fn consume_stream(
        &mut self,
        stream_arn: &str,
        table_name: &str,
        key_attrs: &[String],
        ctx: Arc<dyn SourceSyncContext>,
        mode: SourceCdcMode,
    ) -> Result<(), std::io::Error> {
        let desc = self
            .streams_client
            .describe_stream()
            .stream_arn(stream_arn)
            .send()
            .await
            .map_err(|e| {
                std::io::Error::other(format!("DynamoDB Streams describe_stream failed: {}", e))
            })?;

        let stream_desc = desc
            .stream_description()
            .ok_or_else(|| std::io::Error::other("describe_stream returned no description"))?;

        let shards: Vec<String> = stream_desc
            .shards()
            .iter()
            .filter_map(|s| s.shard_id().map(|id| id.to_string()))
            .collect();

        if shards.is_empty() {
            info!("DynamoDB CDC: no shards found in stream, snapshot only");
            return Ok(());
        }

        info!(
            "DynamoDB CDC: consuming {} shards from stream",
            shards.len()
        );

        for shard_id in &shards {
            self.consume_shard(
                stream_arn,
                shard_id,
                table_name,
                key_attrs,
                ctx.clone(),
                mode,
            )
            .await?;
        }

        Ok(())
    }

    async fn consume_shard(
        &mut self,
        stream_arn: &str,
        shard_id: &str,
        table_name: &str,
        _key_attrs: &[String],
        ctx: Arc<dyn SourceSyncContext>,
        mode: SourceCdcMode,
    ) -> Result<(), std::io::Error> {
        let namespace = format!("dynamodb.{}", table_name);
        let shard_offset_key = OffsetKey {
            namespace: format!("dynamodb-stream:{}:{}", table_name, shard_id),
            partition: "0".to_string(),
        };

        let shard_ckpt_key = format!("dynamodb-stream:{}:{}:seq", table_name, shard_id);
        let stored_seq =
            load_checkpoint_payload::<DynamodbCheckpoint>(ctx.as_ref(), &shard_ckpt_key)
                .map(|checkpoint| checkpoint.sequence_number);

        let mut iter_builder = self
            .streams_client
            .get_shard_iterator()
            .stream_arn(stream_arn)
            .shard_id(shard_id);

        if let Some(ref seq_str) = stored_seq {
            info!(
                "DynamoDB CDC: resuming shard {} from sequence {}",
                shard_id, seq_str
            );
            iter_builder = iter_builder
                .shard_iterator_type(ShardIteratorType::AfterSequenceNumber)
                .sequence_number(seq_str);
        } else {
            iter_builder = iter_builder.shard_iterator_type(initial_shard_iterator_type(mode));
        }

        let iter_resp = iter_builder.send().await.map_err(|e| {
            std::io::Error::other(format!(
                "get_shard_iterator failed for shard {}: {}",
                shard_id, e
            ))
        })?;

        let mut shard_iterator = match iter_resp.shard_iterator() {
            Some(it) => it.to_string(),
            None => {
                warn!("DynamoDB CDC: no iterator for shard {}, skipping", shard_id);
                return Ok(());
            }
        };

        let mut total_records = 0u64;
        let mut empty_polls = 0u32;
        let mut last_seq: Option<String> = None;
        const MAX_EMPTY_POLLS: u32 = 5;

        loop {
            let records_resp = self
                .streams_client
                .get_records()
                .shard_iterator(&shard_iterator)
                .send()
                .await
                .map_err(|e| {
                    std::io::Error::other(format!(
                        "get_records failed for shard {}: {}",
                        shard_id, e
                    ))
                })?;

            let records = records_resp.records();

            if records.is_empty() {
                empty_polls += 1;
                if empty_polls >= MAX_EMPTY_POLLS {
                    info!(
                        "DynamoDB CDC: shard {} exhausted after {} records",
                        shard_id, total_records
                    );
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            } else {
                empty_polls = 0;

                let mut batch: Vec<IngestBatch> = Vec::with_capacity(records.len());

                for record in records {
                    let stream_record = match record.dynamodb() {
                        Some(sr) => sr,
                        None => continue,
                    };

                    let event_name = record.event_name();
                    let mutation = match event_name {
                        Some(&OperationType::Insert) => MutationKind::Insert,
                        Some(&OperationType::Modify) => MutationKind::Update,
                        Some(&OperationType::Remove) => MutationKind::Delete,
                        _ => MutationKind::Insert,
                    };

                    let image: &HashMap<String, aws_sdk_dynamodbstreams::types::AttributeValue> =
                        match mutation {
                            MutationKind::Delete => match stream_record.keys() {
                                Some(keys) if !keys.is_empty() => keys,
                                _ => continue,
                            },
                            _ => match stream_record.new_image() {
                                Some(img) if !img.is_empty() => img,
                                _ => match stream_record.keys() {
                                    Some(keys) if !keys.is_empty() => keys,
                                    _ => continue,
                                },
                            },
                        };

                    let json_str = Self::streams_item_to_json(image);
                    let bytes = json_str.len();

                    let seq_number = stream_record.sequence_number().unwrap_or("0");
                    last_seq = Some(seq_number.to_string());
                    let event_id = seq_number.as_bytes().to_vec();

                    let order_token = Self::sequence_number_to_order_token(seq_number);

                    let cdc_rows = Some(vec![WalRowMeta {
                        mutation,
                        event_id,
                        order_token,
                    }]);

                    batch.push(IngestBatch {
                        offset_key: shard_offset_key.clone(),
                        data: json_str,
                        bytes,
                        offset_pos: None,
                        source_uri: format!("dynamodb-stream://{}/{}", table_name, shard_id),
                        namespace: Some(namespace.clone()),
                        cdc_rows,
                    });

                    total_records += 1;
                }

                if !batch.is_empty() {
                    submit_payload_batches(ctx.as_ref(), batch)?;

                    if let Some(ref seq) = last_seq {
                        store_checkpoint_payload(
                            ctx.as_ref(),
                            &shard_ckpt_key,
                            &DynamodbCheckpoint {
                                table_name: table_name.to_string(),
                                stream_arn: stream_arn.to_string(),
                                shard_id: shard_id.to_string(),
                                sequence_number: seq.clone(),
                            },
                        )?;
                    }
                }
            }

            match records_resp.next_shard_iterator() {
                Some(next) => shard_iterator = next.to_string(),
                None => {
                    info!(
                        "DynamoDB CDC: shard {} closed after {} records",
                        shard_id, total_records
                    );
                    break;
                }
            }
        }

        Ok(())
    }

    fn streams_item_to_json(
        item: &HashMap<String, aws_sdk_dynamodbstreams::types::AttributeValue>,
    ) -> String {
        let mut map = Map::new();
        for (k, v) in item {
            map.insert(k.clone(), Self::streams_attribute_to_json(v));
        }
        serde_json::to_string(&Value::Object(map)).unwrap_or_default()
    }

    fn streams_attribute_to_json(av: &aws_sdk_dynamodbstreams::types::AttributeValue) -> Value {
        use aws_sdk_dynamodbstreams::types::AttributeValue as SAV;
        match av {
            SAV::S(s) => Value::String(s.clone()),
            SAV::N(s) => Self::n_to_json(s),
            SAV::Bool(b) => Value::Bool(*b),
            SAV::Null(true) | SAV::Null(false) => Value::Null,
            SAV::L(list) => {
                Value::Array(list.iter().map(Self::streams_attribute_to_json).collect())
            }
            SAV::M(map) => {
                let mut m = Map::new();
                for (k, v) in map {
                    m.insert(k.clone(), Self::streams_attribute_to_json(v));
                }
                Value::Object(m)
            }
            SAV::B(blob) => Value::String(B64.encode(blob.as_ref())),
            SAV::Ss(ss) => Value::Array(ss.iter().cloned().map(Value::String).collect()),
            SAV::Ns(ns) => Value::Array(ns.iter().map(|s| Self::n_to_json(s)).collect()),
            SAV::Bs(bs) => Value::Array(
                bs.iter()
                    .map(|b| Value::String(B64.encode(b.as_ref())))
                    .collect(),
            ),
            _ => Value::Null,
        }
    }

    fn sequence_number_to_order_token(seq: &str) -> Vec<u8> {
        let numeric: u128 = seq.parse().unwrap_or(0);
        numeric.to_be_bytes().to_vec()
    }
}

fn initial_shard_iterator_type(_mode: SourceCdcMode) -> ShardIteratorType {
    // SnapshotThenCdc already lands the current table state. Replaying stream
    // history from TrimHorizon would apply pre-snapshot mutations again.
    ShardIteratorType::Latest
}

#[async_trait]
impl DataSource for DataSourceDynamodbPlugin {
    async fn sync(&mut self, ctx: Arc<dyn SourceSyncContext>) -> Result<(), std::io::Error> {
        match self.cdc_mode() {
            SourceCdcMode::Snapshot => self.sync_scan(ctx, None).await,
            mode @ (SourceCdcMode::SnapshotThenCdc | SourceCdcMode::CdcOnly) => {
                self.sync_cdc(ctx, mode).await
            }
        }
    }

    fn execution_contract(&self) -> SourceExecutionContract {
        let mode = self.cdc_mode();
        if mode.includes_cdc_stream() {
            SourceExecutionContract::configurable_cdc(
                mode,
                &source_capabilities::DYNAMODB,
                SourceOnceContract::PluginIdleBounded,
            )
        } else {
            SourceExecutionContract::configurable_cdc(
                mode,
                &source_capabilities::DYNAMODB,
                SourceOnceContract::Finite,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_item_key_hash_deterministic() {
        let mut item = HashMap::new();
        item.insert("pk".to_string(), AttributeValue::S("user-42".into()));
        item.insert("sk".to_string(), AttributeValue::S("order-7".into()));

        let key_attrs = vec!["pk".to_string(), "sk".to_string()];

        let hash1 = DataSourceDynamodbPlugin::item_key_hash(&item, &key_attrs);
        let hash2 = DataSourceDynamodbPlugin::item_key_hash(&item, &key_attrs);
        assert_eq!(hash1, hash2, "same keys must produce identical hashes");
        assert_eq!(hash1.len(), 16, "MD5 hash must be 16 bytes");
    }

    #[test]
    fn test_item_key_hash_different_values_differ() {
        let key_attrs = vec!["pk".to_string()];

        let mut item_a = HashMap::new();
        item_a.insert("pk".to_string(), AttributeValue::S("alpha".into()));

        let mut item_b = HashMap::new();
        item_b.insert("pk".to_string(), AttributeValue::S("beta".into()));

        let hash_a = DataSourceDynamodbPlugin::item_key_hash(&item_a, &key_attrs);
        let hash_b = DataSourceDynamodbPlugin::item_key_hash(&item_b, &key_attrs);
        assert_ne!(
            hash_a, hash_b,
            "different key values must produce different hashes"
        );
    }

    #[test]
    fn test_item_key_hash_key_order_matters() {
        let mut item = HashMap::new();
        item.insert("a".to_string(), AttributeValue::S("1".into()));
        item.insert("b".to_string(), AttributeValue::S("2".into()));

        let hash_ab =
            DataSourceDynamodbPlugin::item_key_hash(&item, &["a".to_string(), "b".to_string()]);
        let hash_ba =
            DataSourceDynamodbPlugin::item_key_hash(&item, &["b".to_string(), "a".to_string()]);
        assert_ne!(
            hash_ab, hash_ba,
            "different key_attrs ordering must produce different hashes"
        );
    }

    #[test]
    fn test_sequence_number_to_order_token_big_endian() {
        let token = DataSourceDynamodbPlugin::sequence_number_to_order_token("12345");
        assert_eq!(token.len(), 16, "u128 big-endian should be 16 bytes");
        let restored = u128::from_be_bytes(token.try_into().unwrap());
        assert_eq!(restored, 12345);
    }

    #[test]
    fn test_sequence_number_ordering_preserved() {
        let t1 = DataSourceDynamodbPlugin::sequence_number_to_order_token("100");
        let t2 = DataSourceDynamodbPlugin::sequence_number_to_order_token("200");
        let t3 = DataSourceDynamodbPlugin::sequence_number_to_order_token("999999999999");
        assert!(t1 < t2, "smaller sequence must produce smaller token");
        assert!(t2 < t3, "larger sequence must produce larger token");
    }

    #[test]
    fn snapshot_then_cdc_starts_stream_at_latest_without_checkpoint() {
        assert_eq!(
            initial_shard_iterator_type(SourceCdcMode::SnapshotThenCdc),
            ShardIteratorType::Latest
        );
        assert_eq!(
            initial_shard_iterator_type(SourceCdcMode::CdcOnly),
            ShardIteratorType::Latest
        );
    }

    #[test]
    fn test_sequence_number_zero_fallback() {
        let token = DataSourceDynamodbPlugin::sequence_number_to_order_token("not_a_number");
        let restored = u128::from_be_bytes(token.try_into().unwrap());
        assert_eq!(restored, 0, "non-numeric should fall back to 0");
    }

    #[test]
    fn test_streams_attribute_to_json_string() {
        use aws_sdk_dynamodbstreams::types::AttributeValue as SAV;
        let val = SAV::S("hello".into());
        let json = DataSourceDynamodbPlugin::streams_attribute_to_json(&val);
        assert_eq!(json, Value::String("hello".into()));
    }

    #[test]
    fn test_streams_attribute_to_json_number() {
        use aws_sdk_dynamodbstreams::types::AttributeValue as SAV;
        let val = SAV::N("42".into());
        let json = DataSourceDynamodbPlugin::streams_attribute_to_json(&val);
        assert_eq!(json, json!(42));
    }

    #[test]
    fn test_streams_attribute_to_json_nested_map() {
        use aws_sdk_dynamodbstreams::types::AttributeValue as SAV;
        let mut inner = HashMap::new();
        inner.insert("nested_key".to_string(), SAV::S("nested_val".into()));
        let val = SAV::M(inner);
        let json = DataSourceDynamodbPlugin::streams_attribute_to_json(&val);
        assert_eq!(json["nested_key"], Value::String("nested_val".into()));
    }

    #[test]
    fn test_streams_item_to_json_roundtrip() {
        use aws_sdk_dynamodbstreams::types::AttributeValue as SAV;
        let mut item = HashMap::new();
        item.insert("pk".to_string(), SAV::S("user-1".into()));
        item.insert("count".to_string(), SAV::N("7".into()));
        item.insert("active".to_string(), SAV::Bool(true));

        let json_str = DataSourceDynamodbPlugin::streams_item_to_json(&item);
        let parsed: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["pk"], "user-1");
        assert_eq!(parsed["count"], 7);
        assert_eq!(parsed["active"], true);
    }
}
