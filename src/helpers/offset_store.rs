//! Offset/checkpoint/journal families on Skippr Store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::helpers::offsets::{OffsetTypes, OffsetValue};
use skippr_lease::{decide_wal_publish, OffsetPublishError, PublishAction, PublishMode};
use skippr_wal_catalog::{
    lookup_miss_in_sled, CatalogFence, KvOp, SkipprStore, SkipprStoreRead, StoreFamily,
};
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
    fn get_journal(&self, key: &JournalKey) -> Result<Option<Vec<u8>>, String>;
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

pub struct StoreOffsetStore {
    store: Arc<dyn SkipprStore>,
    fence: CatalogFence,
    sled_fallback: Option<PathBuf>,
}

impl StoreOffsetStore {
    pub fn new(store: Arc<dyn SkipprStore>, fence: CatalogFence) -> Self {
        Self {
            store,
            fence,
            sled_fallback: None,
        }
    }

    pub fn with_sled_fallback(mut self, path: impl AsRef<Path>) -> Self {
        self.sled_fallback = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn kv(&self) -> Arc<dyn SkipprStore> {
        Arc::clone(&self.store)
    }

    pub fn fence(&self) -> CatalogFence {
        self.fence
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

    fn family_get(&self, family: StoreFamily, rest: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let key = family.key(rest);
        if let Some(bytes) = self.store.get(&key).map_err(|err| err.to_string())? {
            return Ok(Some(bytes));
        }
        let Some(sled_path) = self.sled_fallback.as_deref() else {
            return Ok(None);
        };
        lookup_miss_in_sled(sled_path, family, rest).map_err(|err| err.to_string())
    }
}

impl OffsetStore for StoreOffsetStore {
    fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, String> {
        self.family_get(StoreFamily::Offsets, sk.as_bytes())
    }

    fn get_journal(&self, key: &JournalKey) -> Result<Option<Vec<u8>>, String> {
        self.family_get(StoreFamily::Journal, key.as_str().as_bytes())
    }

    fn fetch_and_update_journal(
        &self,
        key: &JournalKey,
        update: Box<dyn FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>> + Send>,
    ) -> Result<Option<Vec<u8>>, String> {
        let rest = key.as_str().as_bytes();
        let current = self.family_get(StoreFamily::Journal, rest)?;
        let next = update(current.clone());
        match next {
            Some(bytes) => {
                self.store
                    .commit(
                        &[KvOp::Put {
                            key: StoreFamily::Journal.key(rest),
                            value: bytes.clone(),
                        }],
                        self.fence,
                    )
                    .map_err(|err| err.to_string())?;
                Ok(Some(bytes))
            }
            None => {
                if current.is_some() {
                    self.store
                        .commit(
                            &[KvOp::Delete {
                                key: StoreFamily::Journal.key(rest),
                            }],
                            self.fence,
                        )
                        .map_err(|err| err.to_string())?;
                }
                Ok(None)
            }
        }
    }

    fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_bytes(&Self::offset_key(namespace, partition))
    }

    fn get_checkpoint(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_bytes(&Self::checkpoint_key(key))
    }

    fn flush(&self) -> Result<(), String> {
        self.store.flush_durable().map_err(|err| err.to_string())
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
            .family_get(StoreFamily::Offsets, fence_sk.as_bytes())
            .map_err(OffsetPublishError::Transient)?
            .and_then(|v| Self::decode_fence(&v));
        let stored_ref = stored.as_ref().map(|(e, s, h)| (*e, *s, h.as_str()));
        match decide_wal_publish(stored_ref, epoch, seq, payload_sha256, mode)? {
            PublishAction::AlreadyApplied => return Ok(()),
            PublishAction::Commit => {}
        }
        let fence_bytes = Self::encode_fence(epoch, seq, payload_sha256);
        self.store
            .commit(
                &[
                    KvOp::Put {
                        key: StoreFamily::Offsets.key(sk.as_bytes()),
                        value: bytes.to_vec(),
                    },
                    KvOp::Put {
                        key: StoreFamily::Offsets.key(fence_sk.as_bytes()),
                        value: fence_bytes,
                    },
                ],
                self.fence,
            )
            .map_err(|err| OffsetPublishError::Transient(err.to_string()))?;
        Ok(())
    }

    fn offset_publish_sk(&self, namespace: &str, partition: &str) -> String {
        Self::offset_key(namespace, partition)
    }

    fn checkpoint_publish_sk(&self, key: &str) -> String {
        Self::checkpoint_key(key)
    }

    fn reset_published_ingest(&self) -> Result<(), String> {
        let page = self
            .store
            .query_prefix(StoreFamily::Offsets.prefix(), None, usize::MAX)
            .map_err(|err| err.to_string())?;
        let mut ops = Vec::new();
        for (key, _) in page {
            ops.push(KvOp::Delete { key });
        }
        if !ops.is_empty() {
            self.store
                .commit(&ops, self.fence)
                .map_err(|err| err.to_string())?;
        }
        self.flush()
    }
}

impl OffsetStore for Arc<dyn OffsetStore> {
    fn get_bytes(&self, sk: &str) -> Result<Option<Vec<u8>>, String> {
        (**self).get_bytes(sk)
    }
    fn get_journal(&self, key: &JournalKey) -> Result<Option<Vec<u8>>, String> {
        (**self).get_journal(key)
    }
    fn fetch_and_update_journal(
        &self,
        key: &JournalKey,
        update: Box<dyn FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>> + Send>,
    ) -> Result<Option<Vec<u8>>, String> {
        (**self).fetch_and_update_journal(key, update)
    }
    fn get_offset(&self, namespace: &str, partition: &str) -> Result<Option<Vec<u8>>, String> {
        (**self).get_offset(namespace, partition)
    }
    fn get_checkpoint(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        (**self).get_checkpoint(key)
    }
    fn flush(&self) -> Result<(), String> {
        (**self).flush()
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
        (**self).publish_wal_commit(sk, bytes, epoch, seq, payload_sha256, mode)
    }
    fn offset_publish_sk(&self, namespace: &str, partition: &str) -> String {
        (**self).offset_publish_sk(namespace, partition)
    }
    fn checkpoint_publish_sk(&self, key: &str) -> String {
        (**self).checkpoint_publish_sk(key)
    }
    fn reset_published_ingest(&self) -> Result<(), String> {
        (**self).reset_published_ingest()
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
pub fn memory_offset_store() -> Arc<dyn OffsetStore> {
    let fence = skippr_wal_catalog::CatalogFence::local();
    Arc::new(StoreOffsetStore::new(
        Arc::new(skippr_wal_catalog::MemorySkipprStore::open(fence)),
        fence,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::offsets::OffsetKey;

    fn mem_offsets() -> StoreOffsetStore {
        let fence = skippr_wal_catalog::CatalogFence::local();
        StoreOffsetStore::new(
            Arc::new(skippr_wal_catalog::MemorySkipprStore::open(fence)),
            fence,
        )
    }

    #[test]
    fn store_offset_store_returns_errors_instead_of_swallowing() {
        let store = mem_offsets();
        let sk = StoreOffsetStore::offset_key("ns", "p");
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
    fn journal_commit_is_not_an_offset_key() {
        let store = mem_offsets();
        let key = JournalKey::migration("ws", "p").unwrap();
        store
            .fetch_and_update_journal(&key, Box::new(|_| Some(b"log".to_vec())))
            .unwrap();
        assert_eq!(
            store.get_journal(&key).unwrap().as_deref(),
            Some(b"log".as_slice())
        );
        assert_eq!(store.get_bytes(key.as_str()).unwrap(), None);
        let sk = StoreOffsetStore::offset_key("ns", "part");
        store
            .publish_wal_commit(&sk, b"off", 1, 1, "h", PublishMode::Fresh)
            .unwrap();
        store.reset_published_ingest().unwrap();
        assert_eq!(store.get_bytes(&sk).unwrap(), None);
        assert_eq!(
            store.get_journal(&key).unwrap().as_deref(),
            Some(b"log".as_slice()),
            "RESET ingest must not delete the journal family"
        );
    }

    #[test]
    fn store_publish_fresh_errors_on_newer_tuple_and_does_not_move_value() {
        let store = mem_offsets();
        let sk = StoreOffsetStore::offset_key("ns", "p");
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
    fn store_publish_same_tuple_same_hash_is_idempotent() {
        let store = mem_offsets();
        let sk = StoreOffsetStore::offset_key("ns", "p");
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
    fn reset_published_ingest_flushes_store() {
        let src = include_str!("offset_store.rs");
        assert!(
            src.contains("query_prefix(StoreFamily::Offsets.prefix()"),
            "reset must prefix-scan offsets family"
        );
        assert!(
            src.contains("self.flush()"),
            "reset must flush before discover may delete payloads"
        );
    }
}
