//! DynamoDB-backed offset/checkpoint materialization for ephemeral runtimes (e.g. Lambda).
//! Linked into `skipprd` only via the `offset-store-dynamodb` feature so the host binary
//! does not pull `aws-sdk-dynamodb` in default builds.

use std::collections::HashMap;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;

use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chrono::Utc;
use once_cell::sync::OnceCell;
use tracing::{info, warn};

fn is_conditional_check_failed(err: &impl aws_sdk_dynamodb::error::ProvideErrorMetadata) -> bool {
    err.code() == Some("ConditionalCheckFailedException")
}

type Job = Box<dyn FnOnce(&tokio::runtime::Runtime, Arc<Client>) + Send>;

struct DynamoIoWorker {
    jobs: SyncSender<Job>,
}

static DYNAMO_WORKER: OnceCell<DynamoIoWorker> = OnceCell::new();

/// Dedicated OS thread owns the Tokio runtime + AWS client (skipprd may already use Tokio).
fn dynamo_worker() -> &'static DynamoIoWorker {
    DYNAMO_WORKER.get_or_init(|| {
        let (job_tx, job_rx) = sync_channel::<Job>(256);
        let (ready_tx, ready_rx) = sync_channel::<()>(1);
        thread::Builder::new()
            .name("skippr-dynamo-offset-io".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build DynamoDB offset store runtime");
                let client = rt.block_on(async {
                    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
                    if let Ok(url) = std::env::var("AWS_ENDPOINT_URL_DYNAMODB") {
                        if !url.trim().is_empty() {
                            loader = loader.endpoint_url(url);
                        }
                    }
                    let shared = loader.load().await;
                    Arc::new(Client::new(&shared))
                });
                let _ = ready_tx.send(());
                while let Ok(job) = job_rx.recv() {
                    job(&rt, client.clone());
                }
            })
            .expect("failed to spawn DynamoDB offset store worker thread");
        ready_rx
            .recv()
            .expect("DynamoDB offset worker failed to start");
        DynamoIoWorker { jobs: job_tx }
    })
}

fn run_on_worker<T, F>(f: F) -> T
where
    T: Send + 'static,
    F: FnOnce(&tokio::runtime::Runtime, Arc<Client>) -> T + Send + 'static,
{
    let (reply_tx, reply_rx): (SyncSender<T>, Receiver<T>) = sync_channel(1);
    dynamo_worker()
        .jobs
        .send(Box::new(move |rt, client| {
            let _ = reply_tx.send(f(rt, client));
        }))
        .expect("DynamoDB offset worker channel closed");
    reply_rx
        .recv()
        .expect("DynamoDB offset worker dropped reply")
}

#[derive(Clone)]
pub struct DynamoDbOffsetStore {
    table: String,
    pk: String,
}

impl DynamoDbOffsetStore {
    pub fn open(
        table: String,
        partition_key: String,
        warn_without_s3_wal: bool,
    ) -> Result<Self, String> {
        if table.is_empty() {
            return Err(
                "SKIPPR_OFFSET_DYNAMODB_TABLE is required when SKIPPR_OFFSET_STORE=dynamodb".into(),
            );
        }
        if warn_without_s3_wal {
            warn!(
                "SKIPPR_OFFSET_STORE=dynamodb without WAL_STORAGE=s3; resume may be incomplete on cold start"
            );
        }
        info!(
            "Opening DynamoDB offset store table={} pk={}",
            table, partition_key
        );
        let _ = dynamo_worker();
        Ok(Self {
            table,
            pk: partition_key,
        })
    }

    pub fn offset_sk(namespace: &str, partition: &str) -> String {
        format!("offset#{}#{}", namespace, partition)
    }

    pub fn checkpoint_sk(logical_key: &str) -> String {
        format!("checkpoint#{}", logical_key)
    }

    pub fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, String> {
        let table = self.table.clone();
        let pk = self.pk.clone();
        let sk = sk.to_string();
        run_on_worker(move |rt, client| {
            rt.block_on(async move {
                let resp = client
                    .get_item()
                    .table_name(&table)
                    .key("PK", AttributeValue::S(pk))
                    .key("SK", AttributeValue::S(sk.clone()))
                    .consistent_read(true)
                    .send()
                    .await;
                match resp {
                    Ok(out) => {
                        let item = match out.item {
                            Some(i) => i,
                            None => return Ok(None),
                        };
                        let payload = item
                            .get("payload_b64")
                            .and_then(|v| v.as_s().ok())
                            .ok_or_else(|| {
                                format!("DynamoDB offset item missing payload_b64 for SK={sk}")
                            })?;
                        B64.decode(payload)
                            .map(Some)
                            .map_err(|e| format!("Invalid payload_b64 for SK={sk}: {e}"))
                    }
                    Err(e) => Err(format!("DynamoDB get_item failed: {e}")),
                }
            })
        })
    }

    pub fn put_bytes(&self, sk: &str, bytes: &[u8]) -> Result<(), String> {
        loop {
            match self.put_bytes_once(sk, bytes)? {
                PublishOutcome::Ok => return Ok(()),
                PublishOutcome::Retry => continue,
            }
        }
    }

    fn put_bytes_once(&self, sk: &str, bytes: &[u8]) -> Result<PublishOutcome, String> {
        let table = self.table.clone();
        let pk = self.pk.clone();
        let sk = sk.to_string();
        let incoming = bytes.to_vec();
        let merge_offsets = sk.starts_with("offset#");
        run_on_worker(move |rt, client| {
            rt.block_on(async move {
                let get = client
                    .get_item()
                    .table_name(&table)
                    .key("PK", AttributeValue::S(pk.clone()))
                    .key("SK", AttributeValue::S(sk.clone()))
                    .consistent_read(true)
                    .send()
                    .await
                    .map_err(|e| format!("DynamoDB get_item failed: {e}"))?;
                let existing = get.item;
                let existing_payload = existing.as_ref().and_then(decode_payload_b64);
                let write_bytes = if merge_offsets {
                    merge_offset_value_bytes(existing_payload.as_deref(), &incoming)?
                } else {
                    incoming
                };
                let fence = existing.as_ref().and_then(read_fence);
                put_item_conditional(
                    client.as_ref(),
                    &table,
                    &pk,
                    &sk,
                    &write_bytes,
                    fence.map(|f| (f.epoch, f.commit_index)),
                    existing.as_ref(),
                )
                .await
            })
        })
    }

    pub fn fetch_and_update_bytes<F>(&self, sk: &str, update: F) -> Result<Option<Vec<u8>>, String>
    where
        F: FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>>,
    {
        let existing = self.get_bytes(sk)?;
        let new_val = update(existing);
        let Some(new_bytes) = new_val else {
            return Ok(None);
        };
        self.put_bytes(sk, &new_bytes)?;
        Ok(Some(new_bytes))
    }

    pub fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_bytes(&Self::offset_sk(namespace, partition))
    }

    pub fn put_offset(&self, namespace: &str, partition: &str, bytes: &[u8]) -> Result<(), String> {
        self.put_bytes(&Self::offset_sk(namespace, partition), bytes)
    }

    pub fn fetch_and_update_offset<F>(
        &self,
        namespace: &str,
        partition: &str,
        update: F,
    ) -> Result<Option<Vec<u8>>, String>
    where
        F: FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>>,
    {
        self.fetch_and_update_bytes(&Self::offset_sk(namespace, partition), update)
    }

    pub fn get_checkpoint(&self, logical_key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_bytes(&Self::checkpoint_sk(logical_key))
    }

    pub fn put_checkpoint(&self, logical_key: &str, bytes: &[u8]) -> Result<(), String> {
        self.put_bytes(&Self::checkpoint_sk(logical_key), bytes)
    }

    /// Clustered publication: conditional write of payload plus WAL epoch/index/hash.
    pub fn publish_wal_commit(
        &self,
        sk: &str,
        bytes: &[u8],
        epoch: u64,
        commit_index: u64,
        payload_sha256: &str,
    ) -> Result<(), String> {
        loop {
            match self.publish_wal_commit_once(sk, bytes, epoch, commit_index, payload_sha256)? {
                PublishOutcome::Ok => return Ok(()),
                PublishOutcome::Retry => continue,
            }
        }
    }

    fn publish_wal_commit_once(
        &self,
        sk: &str,
        bytes: &[u8],
        epoch: u64,
        commit_index: u64,
        payload_sha256: &str,
    ) -> Result<PublishOutcome, String> {
        let table = self.table.clone();
        let pk = self.pk.clone();
        let sk = sk.to_string();
        let incoming = bytes.to_vec();
        let incoming_hash = payload_sha256.to_string();
        let merge_offsets = sk.starts_with("offset#");
        run_on_worker(move |rt, client| {
            rt.block_on(async move {
                let get = client
                    .get_item()
                    .table_name(&table)
                    .key("PK", AttributeValue::S(pk.clone()))
                    .key("SK", AttributeValue::S(sk.clone()))
                    .consistent_read(true)
                    .send()
                    .await
                    .map_err(|e| format!("DynamoDB get_item failed: {e}"))?;
                let existing = get.item;
                if let Some(item) = existing.as_ref() {
                    let old_epoch = attr_u64(item, "wal_epoch");
                    let old_index = attr_u64(item, "wal_commit_index");
                    let old_hash = item
                        .get("payload_sha256")
                        .and_then(|v| v.as_s().ok())
                        .cloned();
                    if let (Some(old_epoch), Some(old_index), Some(old_hash)) =
                        (old_epoch, old_index, old_hash)
                    {
                        if old_epoch == epoch && old_index == commit_index {
                            if old_hash == incoming_hash {
                                return Ok(PublishOutcome::Ok);
                            }
                            return Err(format!(
                                "corrupt offset SK={sk}: same WAL tuple with different hash"
                            ));
                        }
                        if (old_epoch, old_index) > (epoch, commit_index) {
                            return Ok(PublishOutcome::Ok);
                        }
                    }
                }
                let existing_payload = existing.as_ref().and_then(decode_payload_b64);
                let write_bytes = if merge_offsets {
                    merge_offset_value_bytes(existing_payload.as_deref(), &incoming)?
                } else {
                    incoming
                };
                let write_hash = sha256_hex(&write_bytes);
                let payload_b64 = B64.encode(&write_bytes);
                let now = Utc::now().to_rfc3339();
                let mut item = HashMap::new();
                item.insert("PK".to_string(), AttributeValue::S(pk.clone()));
                item.insert("SK".to_string(), AttributeValue::S(sk.clone()));
                item.insert("payload_b64".to_string(), AttributeValue::S(payload_b64));
                item.insert("payload_sha256".to_string(), AttributeValue::S(write_hash));
                item.insert(
                    "wal_epoch".to_string(),
                    AttributeValue::N(epoch.to_string()),
                );
                item.insert(
                    "wal_commit_index".to_string(),
                    AttributeValue::N(commit_index.to_string()),
                );
                item.insert("updated_at".to_string(), AttributeValue::S(now));
                let mut put = client.put_item().table_name(&table).set_item(Some(item));
                if let Some(existing) = existing {
                    if existing.contains_key("wal_epoch") {
                        let old_epoch = attr_u64(&existing, "wal_epoch").unwrap_or(0);
                        let old_index = attr_u64(&existing, "wal_commit_index").unwrap_or(0);
                        let old_hash = existing
                            .get("payload_sha256")
                            .and_then(|v| v.as_s().ok())
                            .cloned()
                            .unwrap_or_default();
                        put = put
                            .condition_expression(
                                "wal_epoch = :e AND wal_commit_index = :i AND payload_sha256 = :h",
                            )
                            .expression_attribute_values(
                                ":e",
                                AttributeValue::N(old_epoch.to_string()),
                            )
                            .expression_attribute_values(
                                ":i",
                                AttributeValue::N(old_index.to_string()),
                            )
                            .expression_attribute_values(":h", AttributeValue::S(old_hash));
                    } else {
                        put = put.condition_expression("attribute_not_exists(wal_epoch)");
                    }
                } else {
                    put = put.condition_expression("attribute_not_exists(PK)");
                }
                match put.send().await {
                    Ok(_) => Ok(PublishOutcome::Ok),
                    Err(err) if is_conditional_check_failed(&err) => Ok(PublishOutcome::Retry),
                    Err(err) => Err(format!("DynamoDB put_item failed: {err}")),
                }
            })
        })
    }
}

fn attr_u64(item: &HashMap<String, AttributeValue>, name: &str) -> Option<u64> {
    item.get(name)
        .and_then(|v| v.as_n().ok())
        .and_then(|n| n.parse().ok())
}

fn decode_payload_b64(item: &HashMap<String, AttributeValue>) -> Option<Vec<u8>> {
    item.get("payload_b64")
        .and_then(|v| v.as_s().ok())
        .and_then(|s| B64.decode(s).ok())
}

struct WalFence {
    epoch: u64,
    commit_index: u64,
    payload_sha256: String,
}

fn read_fence(item: &HashMap<String, AttributeValue>) -> Option<WalFence> {
    Some(WalFence {
        epoch: attr_u64(item, "wal_epoch")?,
        commit_index: attr_u64(item, "wal_commit_index")?,
        payload_sha256: item
            .get("payload_sha256")
            .and_then(|v| v.as_s().ok())
            .cloned()?,
    })
}

async fn put_item_conditional(
    client: &Client,
    table: &str,
    pk: &str,
    sk: &str,
    write_bytes: &[u8],
    stamp: Option<(u64, u64)>,
    existing: Option<&HashMap<String, AttributeValue>>,
) -> Result<PublishOutcome, String> {
    let write_hash = sha256_hex(write_bytes);
    let payload_b64 = B64.encode(write_bytes);
    let now = Utc::now().to_rfc3339();
    let mut item = HashMap::new();
    item.insert("PK".to_string(), AttributeValue::S(pk.to_string()));
    item.insert("SK".to_string(), AttributeValue::S(sk.to_string()));
    item.insert("payload_b64".to_string(), AttributeValue::S(payload_b64));
    item.insert("updated_at".to_string(), AttributeValue::S(now));
    if let Some((epoch, commit_index)) = stamp {
        item.insert("payload_sha256".to_string(), AttributeValue::S(write_hash));
        item.insert(
            "wal_epoch".to_string(),
            AttributeValue::N(epoch.to_string()),
        );
        item.insert(
            "wal_commit_index".to_string(),
            AttributeValue::N(commit_index.to_string()),
        );
    }
    let mut put = client.put_item().table_name(table).set_item(Some(item));
    if let Some(existing) = existing {
        if existing.contains_key("wal_epoch") {
            let old = read_fence(existing)
                .ok_or_else(|| format!("DynamoDB offset item missing fence fields for SK={sk}"))?;
            put = put
                .condition_expression(
                    "wal_epoch = :e AND wal_commit_index = :i AND payload_sha256 = :h",
                )
                .expression_attribute_values(":e", AttributeValue::N(old.epoch.to_string()))
                .expression_attribute_values(":i", AttributeValue::N(old.commit_index.to_string()))
                .expression_attribute_values(":h", AttributeValue::S(old.payload_sha256));
        } else {
            put = put.condition_expression("attribute_not_exists(wal_epoch)");
        }
    } else {
        put = put.condition_expression("attribute_not_exists(PK)");
    }
    match put.send().await {
        Ok(_) => Ok(PublishOutcome::Ok),
        Err(err) if is_conditional_check_failed(&err) => Ok(PublishOutcome::Retry),
        Err(err) => Err(format!("DynamoDB put_item failed: {err}")),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

enum PublishOutcome {
    Ok,
    Retry,
}

/// Merge Closed with OR and Position/Filesize with max. `incoming` and `existing`
/// are 24-byte OffsetValue layouts.
pub fn merge_offset_value_bytes(
    existing: Option<&[u8]>,
    incoming: &[u8],
) -> Result<Vec<u8>, String> {
    if incoming.len() != 24 {
        return Err("incoming offset payload must be 24 bytes".into());
    }
    let Some(existing) = existing else {
        return Ok(incoming.to_vec());
    };
    if existing.len() != 24 {
        return Ok(incoming.to_vec());
    }
    let mut out = incoming.to_vec();
    for (offset, use_or) in [(0, false), (8, false), (16, true)] {
        let old = u64::from_le_bytes(existing[offset..offset + 8].try_into().unwrap());
        let new = u64::from_le_bytes(incoming[offset..offset + 8].try_into().unwrap());
        let merged = if use_or { old | new } else { old.max(new) };
        out[offset..offset + 8].copy_from_slice(&merged.to_le_bytes());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::DynamoDbOffsetStore;

    #[test]
    fn dynamo_sk_formats_are_stable() {
        assert_eq!(
            DynamoDbOffsetStore::offset_sk("google_analytics.events", "2024-01-01"),
            "offset#google_analytics.events#2024-01-01"
        );
        assert_eq!(
            DynamoDbOffsetStore::checkpoint_sk("ga4:last_completed_date"),
            "checkpoint#ga4:last_completed_date"
        );
    }

    #[test]
    fn merge_offset_or_closed_and_max_position() {
        let mut existing = vec![0u8; 24];
        existing[8..16].copy_from_slice(&10u64.to_le_bytes());
        existing[16..24].copy_from_slice(&1u64.to_le_bytes());
        let mut incoming = vec![0u8; 24];
        incoming[8..16].copy_from_slice(&7u64.to_le_bytes());
        incoming[16..24].copy_from_slice(&0u64.to_le_bytes());
        let merged = super::merge_offset_value_bytes(Some(&existing), &incoming).unwrap();
        assert_eq!(&merged[8..16], &10u64.to_le_bytes());
        assert_eq!(&merged[16..24], &1u64.to_le_bytes());
    }

    #[test]
    fn clustered_closed_defaults_to_zero() {
        let incoming = vec![0u8; 24];
        let merged = super::merge_offset_value_bytes(None, &incoming).unwrap();
        assert_eq!(&merged[16..24], &0u64.to_le_bytes());
        let existing = vec![0u8; 24];
        let merged = super::merge_offset_value_bytes(Some(&existing), &incoming).unwrap();
        assert_eq!(&merged[16..24], &0u64.to_le_bytes());
    }

    #[test]
    fn put_bytes_merges_offsets_so_closed_survives_position_write() {
        let mut existing = vec![0u8; 24];
        existing[8..16].copy_from_slice(&10u64.to_le_bytes());
        existing[16..24].copy_from_slice(&1u64.to_le_bytes());
        let mut incoming = vec![0u8; 24];
        incoming[8..16].copy_from_slice(&42u64.to_le_bytes());
        let merged = super::merge_offset_value_bytes(Some(&existing), &incoming).unwrap();
        assert_eq!(&merged[8..16], &42u64.to_le_bytes());
        assert_eq!(&merged[16..24], &1u64.to_le_bytes());
    }
}
