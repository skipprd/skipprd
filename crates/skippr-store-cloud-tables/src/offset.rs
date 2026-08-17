use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chrono::Utc;
use once_cell::sync::OnceCell;
use serde_json::{json, Value};
use skippr_tables_client::{attr_n, attr_s, n, s, TablesClient};
use tracing::{info, warn};

type Job = Box<dyn FnOnce(&tokio::runtime::Runtime, Arc<TablesClient>) + Send>;

struct CloudIoWorker {
    jobs: SyncSender<Job>,
}

static CLOUD_WORKER: OnceCell<CloudIoWorker> = OnceCell::new();

fn cloud_worker() -> Result<&'static CloudIoWorker, String> {
    CLOUD_WORKER.get_or_try_init(|| {
        let (job_tx, job_rx) = sync_channel::<Job>(256);
        let (ready_tx, ready_rx) = sync_channel::<Result<Arc<TablesClient>, String>>(1);
        thread::Builder::new()
            .name("skippr-cloud-tables-offset-io".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build Cloud tables offset store runtime");
                let client = match TablesClient::from_env() {
                    Ok(client) => Arc::new(client),
                    Err(err) => {
                        let _ = ready_tx.send(Err(err.to_string()));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(client.clone()));
                while let Ok(job) = job_rx.recv() {
                    job(&rt, client.clone());
                }
            })
            .map_err(|err| err.to_string())?;
        let client = ready_rx
            .recv()
            .map_err(|_| "Cloud tables offset worker failed to start".to_string())??;
        let _ = client;
        Ok(CloudIoWorker { jobs: job_tx })
    })
}

fn run_on_worker<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&tokio::runtime::Runtime, Arc<TablesClient>) -> T + Send + 'static,
{
    let (reply_tx, reply_rx): (SyncSender<T>, Receiver<T>) = sync_channel(1);
    cloud_worker()?
        .jobs
        .send(Box::new(move |rt, client| {
            let _ = reply_tx.send(f(rt, client));
        }))
        .map_err(|_| "Cloud tables offset worker channel closed".to_string())?;
    reply_rx
        .recv()
        .map_err(|_| "Cloud tables offset worker dropped reply".to_string())
}

#[derive(Clone)]
pub struct CloudTablesOffsetStore {
    table: String,
    pk: String,
}

impl CloudTablesOffsetStore {
    pub fn open(
        table: String,
        partition_key: String,
        warn_without_s3_wal: bool,
    ) -> Result<Self, String> {
        if table.is_empty() {
            return Err(
                "SKIPPR_OFFSET_DYNAMODB_TABLE is required when SKIPPR_OFFSET_STORE=cloud-tables"
                    .into(),
            );
        }
        if warn_without_s3_wal {
            warn!(
                "SKIPPR_OFFSET_STORE=cloud-tables without WAL_STORAGE=s3; resume may be incomplete on cold start"
            );
        }
        info!(
            "Opening Cloud tables offset store table={} pk={}",
            table, partition_key
        );
        let _ = cloud_worker()?;
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
                let item = client
                    .get_item(&table, &pk, &sk, true)
                    .await
                    .map_err(|err| format!("Cloud tables get_item failed: {err}"))?;
                let Some(item) = item else {
                    return Ok(None);
                };
                let payload = attr_s(&item, "payload_b64").ok_or_else(|| {
                    format!("Cloud tables offset item missing payload_b64 for SK={sk}")
                })?;
                B64.decode(payload)
                    .map(Some)
                    .map_err(|e| format!("Invalid payload_b64 for SK={sk}: {e}"))
            })
        })?
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
                let existing = client
                    .get_item(&table, &pk, &sk, true)
                    .await
                    .map_err(|e| format!("Cloud tables get_item failed: {e}"))?;
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
        })?
    }

    pub fn fetch_and_update_bytes<F>(&self, sk: &str, update: F) -> Result<Option<Vec<u8>>, String>
    where
        F: FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>>,
    {
        let existing = self.get_bytes(sk)?;
        let Some(new_bytes) = update(existing) else {
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
                let existing = client
                    .get_item(&table, &pk, &sk, true)
                    .await
                    .map_err(|e| format!("Cloud tables get_item failed: {e}"))?;
                if let Some(item) = existing.as_ref() {
                    let old_epoch = attr_n(item, "wal_epoch");
                    let old_index = attr_n(item, "wal_commit_index");
                    let old_hash = attr_s(item, "payload_sha256");
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
                put_wal_commit(
                    client.as_ref(),
                    &table,
                    &pk,
                    &sk,
                    &write_bytes,
                    &write_hash,
                    epoch,
                    commit_index,
                    existing.as_ref(),
                )
                .await
            })
        })?
    }
}

fn decode_payload_b64(item: &Value) -> Option<Vec<u8>> {
    attr_s(item, "payload_b64").and_then(|s| B64.decode(s).ok())
}

struct WalFence {
    epoch: u64,
    commit_index: u64,
    payload_sha256: String,
}

fn read_fence(item: &Value) -> Option<WalFence> {
    Some(WalFence {
        epoch: attr_n(item, "wal_epoch")?,
        commit_index: attr_n(item, "wal_commit_index")?,
        payload_sha256: attr_s(item, "payload_sha256")?,
    })
}

async fn put_item_conditional(
    client: &TablesClient,
    table: &str,
    pk: &str,
    sk: &str,
    write_bytes: &[u8],
    stamp: Option<(u64, u64)>,
    existing: Option<&Value>,
) -> Result<PublishOutcome, String> {
    let write_hash = sha256_hex(write_bytes);
    let payload_b64 = B64.encode(write_bytes);
    let now = Utc::now().to_rfc3339();
    let mut item = json!({
        "PK": s(pk),
        "SK": s(sk),
        "payload_b64": s(payload_b64),
        "updated_at": s(now),
    });
    if let Some((epoch, commit_index)) = stamp {
        item["payload_sha256"] = s(write_hash);
        item["wal_epoch"] = n(epoch);
        item["wal_commit_index"] = n(commit_index);
    }
    let (condition, values) = existing
        .map(|existing| {
            if existing.get("wal_epoch").is_some() {
                let old = read_fence(existing).expect("fence");
                (
                    Some("wal_epoch = :e AND wal_commit_index = :i AND payload_sha256 = :h"),
                    Some(json!({
                        ":e": n(old.epoch),
                        ":i": n(old.commit_index),
                        ":h": s(old.payload_sha256),
                    })),
                )
            } else {
                (Some("attribute_not_exists(wal_epoch)"), None)
            }
        })
        .unwrap_or((Some("attribute_not_exists(PK)"), None));
    match client.put_item(table, item, condition, values).await {
        Ok(()) => Ok(PublishOutcome::Ok),
        Err(err) if err.is_conditional_check_failed() => Ok(PublishOutcome::Retry),
        Err(err) => Err(format!("Cloud tables put_item failed: {err}")),
    }
}

async fn put_wal_commit(
    client: &TablesClient,
    table: &str,
    pk: &str,
    sk: &str,
    write_bytes: &[u8],
    write_hash: &str,
    epoch: u64,
    commit_index: u64,
    existing: Option<&Value>,
) -> Result<PublishOutcome, String> {
    let payload_b64 = B64.encode(write_bytes);
    let now = Utc::now().to_rfc3339();
    let item = json!({
        "PK": s(pk),
        "SK": s(sk),
        "payload_b64": s(payload_b64),
        "payload_sha256": s(write_hash),
        "wal_epoch": n(epoch),
        "wal_commit_index": n(commit_index),
        "updated_at": s(now),
    });
    let (condition, values) = if let Some(existing) = existing {
        if existing.get("wal_epoch").is_some() {
            let old_epoch = attr_n(existing, "wal_epoch").unwrap_or(0);
            let old_index = attr_n(existing, "wal_commit_index").unwrap_or(0);
            let old_hash = attr_s(existing, "payload_sha256").unwrap_or_default();
            (
                Some("wal_epoch = :e AND wal_commit_index = :i AND payload_sha256 = :h"),
                Some(json!({
                    ":e": n(old_epoch),
                    ":i": n(old_index),
                    ":h": s(old_hash),
                })),
            )
        } else {
            (Some("attribute_not_exists(wal_epoch)"), None)
        }
    } else {
        (Some("attribute_not_exists(PK)"), None)
    };
    match client.put_item(table, item, condition, values).await {
        Ok(()) => Ok(PublishOutcome::Ok),
        Err(err) if err.is_conditional_check_failed() => Ok(PublishOutcome::Retry),
        Err(err) => Err(format!("Cloud tables put_item failed: {err}")),
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
