//! Object-safe offset/checkpoint store. Sled, DynamoDB, and Cloud Tables
//! implement this contract; core WAL code does not branch on backend.

use crate::helpers::offsets::{OffsetTypes, OffsetValue};
use skippr_lease::{decide_wal_publish, OffsetPublishError, PublishAction, PublishMode};
use sled::IVec;
use zerocopy::{byteorder::U64, AsBytes, LayoutVerified};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalKey(String);

impl JournalKey {
    pub fn migration(workspace: &str, pipeline: &str) -> Result<Self, String> {
        if workspace.is_empty() || pipeline.is_empty() {
            return Err("journal identity must not be empty".into());
        }
        if workspace.contains('#') || pipeline.contains('#') {
            return Err("journal identity must not contain '#'".into());
        }
        Ok(Self(format!("migration#{workspace}#{pipeline}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub trait OffsetStore: Send + Sync {
    fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, String>;
    fn fetch_and_update_journal(
        &self,
        key: &JournalKey,
        update: Box<dyn FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>> + Send>,
    ) -> Result<Option<Vec<u8>>, String>;
    fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, String>;
    fn get_checkpoint(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    fn flush(&self) -> Result<(), String>;
    fn publish_wal_commit(
        &self,
        sk: &str,
        bytes: &[u8],
        epoch: u64,
        seq: u64,
        payload_sha256: &str,
        mode: PublishMode,
    ) -> Result<(), OffsetPublishError>;
    fn offset_publish_sk(&self, namespace: &str, partition: &str) -> String;
    fn checkpoint_publish_sk(&self, key: &str) -> String;
    fn reset_published_ingest(&self) -> Result<(), String>;
}

pub fn publish_committed(
    store: &dyn OffsetStore,
    offsets: &[skippr_wal_catalog::CommittedOffset],
    checkpoints: &[skippr_wal_catalog::CommittedCheckpoint],
    epoch: u64,
    seq: u64,
    mode: PublishMode,
) -> Result<(), OffsetPublishError> {
    for offset in offsets {
        let sk = store.offset_publish_sk(&offset.namespace, &offset.partition);
        let mut bytes = Vec::with_capacity(24);
        bytes.extend_from_slice(&offset.filesize.to_le_bytes());
        bytes.extend_from_slice(&offset.position.to_le_bytes());
        bytes.extend_from_slice(&offset.closed.to_le_bytes());
        let sha = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&bytes))
        };
        store.publish_wal_commit(&sk, &bytes, epoch, seq, &sha, mode)?;
    }
    for checkpoint in checkpoints {
        let sk = store.checkpoint_publish_sk(&checkpoint.logical_key);
        let sha = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&checkpoint.envelope))
        };
        store.publish_wal_commit(&sk, &checkpoint.envelope, epoch, seq, &sha, mode)?;
    }
    Ok(())
}

pub struct SledOffsetStore {
    tree: sled::Tree,
}

impl SledOffsetStore {
    pub fn new(tree: sled::Tree) -> Self {
        Self { tree }
    }

    pub(crate) fn offset_key(namespace: &str, partition: &str) -> String {
        format!("{namespace}-{partition}")
    }

    fn checkpoint_key(key: &str) -> String {
        format!("cdc_checkpoint:{key}")
    }

    fn fence_key(sk: &str) -> String {
        format!("fence#{sk}")
    }

    fn encode_fence(epoch: u64, seq: u64, payload_sha256: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + payload_sha256.len());
        out.extend_from_slice(&epoch.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(payload_sha256.as_bytes());
        out
    }

    fn decode_fence(bytes: &[u8]) -> Option<(u64, u64, String)> {
        if bytes.len() < 16 {
            return None;
        }
        let epoch = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        let seq = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        let hash = std::str::from_utf8(&bytes[16..]).ok()?.to_string();
        Some((epoch, seq, hash))
    }
}

impl OffsetStore for SledOffsetStore {
    fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, String> {
        self.tree
            .get(sk.as_bytes())
            .map(|v| v.map(|bytes| bytes.to_vec()))
            .map_err(|err| err.to_string())
    }

    fn fetch_and_update_journal(
        &self,
        key: &JournalKey,
        update: Box<dyn FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>> + Send>,
    ) -> Result<Option<Vec<u8>>, String> {
        let mut update = Some(update);
        self.tree
            .fetch_and_update(key.as_str().as_bytes(), |current| {
                update
                    .take()
                    .and_then(|f| f(current.map(|v| v.to_vec())))
                    .map(IVec::from)
            })
            .map(|v| v.map(|bytes| bytes.to_vec()))
            .map_err(|err| err.to_string())
    }

    fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_bytes(&Self::offset_key(namespace, partition))
    }

    fn get_checkpoint(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_bytes(&Self::checkpoint_key(key))
    }

    fn flush(&self) -> Result<(), String> {
        self.tree.flush().map(|_| ()).map_err(|err| err.to_string())
    }

    fn publish_wal_commit(
        &self,
        sk: &str,
        bytes: &[u8],
        epoch: u64,
        seq: u64,
        payload_sha256: &str,
        mode: PublishMode,
    ) -> Result<(), OffsetPublishError> {
        let fence_sk = Self::fence_key(sk);
        let stored = self
            .tree
            .get(fence_sk.as_bytes())
            .map_err(|err| OffsetPublishError::Transient(err.to_string()))?
            .and_then(|v| Self::decode_fence(&v));
        let stored_ref = stored.as_ref().map(|(e, s, h)| (*e, *s, h.as_str()));
        match decide_wal_publish(stored_ref, epoch, seq, payload_sha256, mode)? {
            PublishAction::AlreadyApplied => return Ok(()),
            PublishAction::Commit => {}
        }
        let fence_bytes = Self::encode_fence(epoch, seq, payload_sha256);
        let value = bytes.to_vec();
        self.tree
            .transaction(|tx| {
                tx.insert(sk.as_bytes(), value.as_slice())?;
                tx.insert(fence_sk.as_bytes(), fence_bytes.as_slice())?;
                Ok(())
            })
            .map_err(|err: sled::transaction::TransactionError| {
                OffsetPublishError::Transient(err.to_string())
            })?;
        Ok(())
    }

    fn offset_publish_sk(&self, namespace: &str, partition: &str) -> String {
        Self::offset_key(namespace, partition)
    }

    fn checkpoint_publish_sk(&self, key: &str) -> String {
        Self::checkpoint_key(key)
    }

    fn reset_published_ingest(&self) -> Result<(), String> {
        let mut batch = sled::Batch::default();
        for item in self.tree.iter() {
            let (key, _) = item.map_err(|err| err.to_string())?;
            if key.starts_with(b"migration#") {
                continue;
            }
            batch.remove(key);
        }
        self.tree
            .apply_batch(batch)
            .map_err(|err| err.to_string())?;
        self.flush()
    }
}

#[cfg(feature = "offset-store-dynamodb")]
impl OffsetStore for skippr_offset_store_dynamodb::DynamoDbOffsetStore {
    fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::get_bytes(self, sk)
    }

    fn fetch_and_update_journal(
        &self,
        key: &JournalKey,
        update: Box<dyn FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>> + Send>,
    ) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::fetch_and_update_journal(
            self,
            key.as_str(),
            update,
        )
    }

    fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::get_offset(self, namespace, partition)
    }

    fn get_checkpoint(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::get_checkpoint(self, key)
    }

    fn flush(&self) -> Result<(), String> {
        Ok(())
    }

    fn publish_wal_commit(
        &self,
        sk: &str,
        bytes: &[u8],
        epoch: u64,
        seq: u64,
        payload_sha256: &str,
        mode: PublishMode,
    ) -> Result<(), OffsetPublishError> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::publish_wal_commit(
            self,
            sk,
            bytes,
            epoch,
            seq,
            payload_sha256,
            mode,
        )
    }

    fn offset_publish_sk(&self, namespace: &str, partition: &str) -> String {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::offset_sk(namespace, partition)
    }

    fn checkpoint_publish_sk(&self, key: &str) -> String {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::checkpoint_sk(key)
    }

    fn reset_published_ingest(&self) -> Result<(), String> {
        skippr_offset_store_dynamodb::DynamoDbOffsetStore::reset_published_ingest(self)
    }
}

#[cfg(feature = "offset-store-cloud-tables")]
impl OffsetStore for skippr_offset_store_cloud_tables::CloudTablesOffsetStore {
    fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_cloud_tables::CloudTablesOffsetStore::get_bytes(self, sk)
    }

    fn fetch_and_update_journal(
        &self,
        key: &JournalKey,
        update: Box<dyn FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>> + Send>,
    ) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_cloud_tables::CloudTablesOffsetStore::fetch_and_update_journal(
            self,
            key.as_str(),
            update,
        )
    }

    fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_cloud_tables::CloudTablesOffsetStore::get_offset(
            self, namespace, partition,
        )
    }

    fn get_checkpoint(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        skippr_offset_store_cloud_tables::CloudTablesOffsetStore::get_checkpoint(self, key)
    }

    fn flush(&self) -> Result<(), String> {
        Ok(())
    }

    fn publish_wal_commit(
        &self,
        sk: &str,
        bytes: &[u8],
        epoch: u64,
        seq: u64,
        payload_sha256: &str,
        mode: PublishMode,
    ) -> Result<(), OffsetPublishError> {
        skippr_offset_store_cloud_tables::CloudTablesOffsetStore::publish_wal_commit(
            self,
            sk,
            bytes,
            epoch,
            seq,
            payload_sha256,
            mode,
        )
    }

    fn offset_publish_sk(&self, namespace: &str, partition: &str) -> String {
        skippr_offset_store_cloud_tables::CloudTablesOffsetStore::offset_sk(namespace, partition)
    }

    fn checkpoint_publish_sk(&self, key: &str) -> String {
        skippr_offset_store_cloud_tables::CloudTablesOffsetStore::checkpoint_sk(key)
    }

    fn reset_published_ingest(&self) -> Result<(), String> {
        skippr_offset_store_cloud_tables::CloudTablesOffsetStore::reset_published_ingest(self)
    }
}

pub fn apply_offset_field(
    existing: Option<Vec<u8>>,
    offset_type: OffsetTypes,
    offset: u64,
    position_max: bool,
) -> Result<Vec<u8>, String> {
    let mut backing = existing.unwrap_or_else(|| {
        OffsetValue {
            filesize: U64::new(0),
            line: U64::new(0),
            closed: U64::new(0),
        }
        .as_bytes()
        .to_vec()
    });
    let layout: LayoutVerified<&mut [u8], OffsetValue> =
        LayoutVerified::new_unaligned(backing.as_mut_slice())
            .ok_or_else(|| "offset bytes do not fit schema".to_string())?;
    let value: &mut OffsetValue = layout.into_mut();
    match offset_type {
        OffsetTypes::Filesize => value.filesize = U64::new(offset),
        OffsetTypes::Position => {
            let current = value.line.get();
            value.line = U64::new(if position_max {
                current.max(offset)
            } else {
                offset
            });
        }
        OffsetTypes::Closed => value.closed = U64::new(offset),
    }
    Ok(backing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::offsets::OffsetKey;

    #[test]
    fn sled_offset_store_returns_errors_instead_of_swallowing() {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let store = SledOffsetStore::new(db.open_tree("offsets").unwrap());
        let sk = SledOffsetStore::offset_key("ns", "p");
        let bytes = apply_offset_field(None, OffsetTypes::Closed, 1, false).unwrap();
        let sha = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&bytes))
        };
        store
            .publish_wal_commit(&sk, &bytes, 1, 1, &sha, PublishMode::Fresh)
            .unwrap();
        assert!(store.get_bytes(&sk).unwrap().is_some());
        assert!(store.get_offset("ns", "p").unwrap().is_some());
        let _ = OffsetKey::new("ns", "p");
    }

    #[test]
    fn apply_offset_field_rejects_corrupt_schema() {
        let err =
            apply_offset_field(Some(vec![1, 2, 3]), OffsetTypes::Closed, 1, false).unwrap_err();
        assert_eq!(err, "offset bytes do not fit schema");
    }

    #[test]
    fn sled_publish_fresh_errors_on_newer_tuple_and_does_not_move_value() {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let store = SledOffsetStore::new(db.open_tree("offsets").unwrap());
        let sk = SledOffsetStore::offset_key("ns", "p");
        store
            .publish_wal_commit(&sk, b"newer", 2, 10, "h2", PublishMode::Fresh)
            .unwrap();
        let err = store
            .publish_wal_commit(&sk, b"older", 1, 9, "h1", PublishMode::Fresh)
            .unwrap_err();
        assert_eq!(err, OffsetPublishError::FenceRegression);
        assert_eq!(
            store.get_bytes(&sk).unwrap().as_deref(),
            Some(b"newer".as_slice())
        );
        store
            .publish_wal_commit(&sk, b"older", 1, 9, "h1", PublishMode::Republish)
            .unwrap();
        assert_eq!(
            store.get_bytes(&sk).unwrap().as_deref(),
            Some(b"newer".as_slice())
        );
    }

    #[test]
    fn sled_publish_same_tuple_same_hash_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = sled::open(dir.path()).unwrap();
        let store = SledOffsetStore::new(db.open_tree("offsets").unwrap());
        let sk = SledOffsetStore::offset_key("ns", "p");
        store
            .publish_wal_commit(&sk, b"v", 1, 4, "abc", PublishMode::Fresh)
            .unwrap();
        store
            .publish_wal_commit(&sk, b"v", 1, 4, "abc", PublishMode::Fresh)
            .unwrap();
        assert_eq!(
            store.get_bytes(&sk).unwrap().as_deref(),
            Some(b"v".as_slice())
        );
    }

    #[test]
    fn offsets_dispatch_is_the_offset_store_trait() {
        let offsets = include_str!("offsets.rs");
        assert!(
            offsets.contains("Arc<dyn OffsetStore>"),
            "Offsets must dispatch through dyn OffsetStore"
        );
        assert!(!offsets.contains(concat!("pub fn insert", "(")));
        assert!(!offsets.contains(concat!("pub fn set", "(")));
        assert!(!offsets.contains(concat!("pub fn upsert", "(")));
        assert!(!offsets.contains(concat!("pub fn store_checkpoint", "_envelope")));
    }

    #[test]
    fn reset_published_ingest_flushes_sled() {
        let src = include_str!("offset_store.rs");
        let sled_reset = src
            .split("fn reset_published_ingest")
            .nth(2)
            .expect("SledOffsetStore::reset_published_ingest");
        let body = sled_reset.split("fn ").next().unwrap();
        assert!(
            body.contains("apply_batch"),
            "sled reset must batch-delete published ingest keys"
        );
        assert!(
            body.contains("flush("),
            "sled reset must flush before discover may delete payloads"
        );
    }
}
