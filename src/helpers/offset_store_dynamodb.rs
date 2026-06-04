//! Optional DynamoDB-backed offset/checkpoint store for short-lived runtimes (e.g. Lambda).
//! Committed S3 WAL segments remain the ownership proof; this store materializes resume state.

use std::sync::Arc;

use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chrono::Utc;
use once_cell::sync::OnceCell;
use tracing::{info, warn};

use crate::helpers::configuration::Config;
use crate::helpers::offsets::OffsetsError;

static DYNAMO_CLIENT: OnceCell<Arc<Client>> = OnceCell::new();

fn dynamo_client() -> Arc<Client> {
    DYNAMO_CLIENT
        .get_or_init(|| {
            let rt = tokio::runtime::Handle::try_current()
                .expect("DynamoDB offset store requires a Tokio runtime");
            rt.block_on(async {
                let shared = aws_config::defaults(aws_config::BehaviorVersion::latest())
                    .load()
                    .await;
                Arc::new(Client::new(&shared))
            })
        })
        .clone()
}

#[derive(Clone)]
pub struct DynamoDbOffsetStore {
    client: Arc<Client>,
    table: String,
    pk: String,
}

impl DynamoDbOffsetStore {
    pub fn open() -> Result<Self, OffsetsError> {
        let table = Config::get_offset_dynamodb_table();
        if table.is_empty() {
            return Err(OffsetsError::AlreadyOpenError(
                "SKIPPR_OFFSET_DYNAMODB_TABLE is required when SKIPPR_OFFSET_STORE=dynamodb".into(),
            ));
        }
        if !Config::get_wal_storage().eq_ignore_ascii_case("s3") {
            warn!(
                "SKIPPR_OFFSET_STORE=dynamodb without WAL_STORAGE=s3; resume may be incomplete on cold start"
            );
        }
        let pk = Config::offset_store_partition_key();
        info!(
            "Opening DynamoDB offset store table={} pk={}",
            table, pk
        );
        Ok(Self {
            client: dynamo_client(),
            table,
            pk,
        })
    }

    pub(crate) fn offset_sk(namespace: &str, partition: &str) -> String {
        format!("offset#{}#{}", namespace, partition)
    }

    pub(crate) fn checkpoint_sk(logical_key: &str) -> String {
        format!("checkpoint#{}", logical_key)
    }

    pub fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, OffsetsError> {
        let resp = tokio::runtime::Handle::current().block_on(async {
            self.client
                .get_item()
                .table_name(&self.table)
                .key("PK", AttributeValue::S(self.pk.clone()))
                .key("SK", AttributeValue::S(sk.to_string()))
                .send()
                .await
        });
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
                        OffsetsError::AlreadyOpenError(format!(
                            "DynamoDB offset item missing payload_b64 for SK={}",
                            sk
                        ))
                    })?;
                let bytes = B64.decode(payload).map_err(|e| {
                    OffsetsError::AlreadyOpenError(format!(
                        "Invalid payload_b64 for SK={}: {}",
                        sk, e
                    ))
                })?;
                Ok(Some(bytes))
            }
            Err(e) => Err(OffsetsError::AlreadyOpenError(format!(
                "DynamoDB get_item failed: {}",
                e
            ))),
        }
    }

    pub fn put_bytes(&self, sk: &str, bytes: &[u8]) -> Result<(), OffsetsError> {
        let now = Utc::now().to_rfc3339();
        let payload_b64 = B64.encode(bytes);
        tokio::runtime::Handle::current()
            .block_on(async {
                self.client
                    .put_item()
                    .table_name(&self.table)
                    .item("PK", AttributeValue::S(self.pk.clone()))
                    .item("SK", AttributeValue::S(sk.to_string()))
                    .item("payload_b64", AttributeValue::S(payload_b64))
                    .item("updated_at", AttributeValue::S(now))
                    .send()
                    .await
            })
            .map_err(|e| {
                OffsetsError::AlreadyOpenError(format!("DynamoDB put_item failed: {}", e))
            })?;
        Ok(())
    }

    pub fn fetch_and_update_bytes<F>(&self, sk: &str, update: F) -> Result<Option<Vec<u8>>, OffsetsError>
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

    pub fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, OffsetsError> {
        self.get_bytes(&Self::offset_sk(namespace, partition))
    }

    pub fn put_offset(
        &self,
        namespace: &str,
        partition: &str,
        bytes: &[u8],
    ) -> Result<(), OffsetsError> {
        self.put_bytes(&Self::offset_sk(namespace, partition), bytes)
    }

    pub fn fetch_and_update_offset<F>(
        &self,
        namespace: &str,
        partition: &str,
        update: F,
    ) -> Result<Option<Vec<u8>>, OffsetsError>
    where
        F: FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>>,
    {
        self.fetch_and_update_bytes(&Self::offset_sk(namespace, partition), update)
    }

    pub fn get_checkpoint(&self, logical_key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_bytes(&Self::checkpoint_sk(logical_key))
            .map_err(|e| e.to_string())
    }

    pub fn put_checkpoint(&self, logical_key: &str, bytes: &[u8]) -> Result<(), String> {
        self.put_bytes(&Self::checkpoint_sk(logical_key), bytes)
            .map_err(|e| e.to_string())
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
