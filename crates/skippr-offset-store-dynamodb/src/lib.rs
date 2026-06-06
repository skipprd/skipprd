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
                    let shared = aws_config::defaults(aws_config::BehaviorVersion::latest())
                        .load()
                        .await;
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
        let table = self.table.clone();
        let pk = self.pk.clone();
        let sk = sk.to_string();
        let payload_b64 = B64.encode(bytes);
        run_on_worker(move |rt, client| {
            rt.block_on(async move {
                let now = Utc::now().to_rfc3339();
                let mut item = HashMap::new();
                item.insert("PK".to_string(), AttributeValue::S(pk));
                item.insert("SK".to_string(), AttributeValue::S(sk));
                item.insert("payload_b64".to_string(), AttributeValue::S(payload_b64));
                item.insert("updated_at".to_string(), AttributeValue::S(now));
                client
                    .put_item()
                    .table_name(&table)
                    .set_item(Some(item))
                    .send()
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("DynamoDB put_item failed: {e}"))
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
}
