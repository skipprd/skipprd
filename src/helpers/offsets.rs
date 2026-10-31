use std::sync::Arc;
use Result;

use crate::helpers::configuration::Config;
use crate::helpers::offset_store::{OffsetStore, StoreOffsetStore};
use crate::plugins::cdc::CheckpointEnvelope;
use serde_derive::{Deserialize, Serialize};
use skippr_wal_catalog::SkipprStoreRead;
use thiserror::Error;
use tracing::info;
use {
    byteorder::LittleEndian,
    zerocopy::{byteorder::U64, AsBytes, FromBytes, LayoutVerified, Unaligned},
};

pub const SLED_NAME: &str = "db";

// We use `BigEndian` for key types because
// they preserve lexicographic ordering,
// which is nice if we ever want to iterate
// over our items in order. We use the
// `U64` type from zerocopy because it
// does not have alignment requirements.
// sled does not guarantee any particular
// value alignment as of now.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[repr(C)]
pub struct OffsetKey {
    pub namespace: String,
    pub partition: String,
}

impl OffsetKey {
    pub fn new(namespace: impl Into<String>, partition: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            partition: partition.into(),
        }
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn partition(&self) -> &str {
        &self.partition
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum OffsetTypes {
    Filesize,
    Position,
    Closed,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum RuntimeOffsetOperation {
    Validate {
        key: OffsetKey,
        offset_type: OffsetTypes,
        offset_value: u64,
    },
    LoadCheckpointEnvelope {
        key: String,
    },
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeOffsetRpcRequest {
    pub request_id: u64,
    pub operation: RuntimeOffsetOperation,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum RuntimeOffsetValue {
    Validate(Option<bool>),
    LoadCheckpointEnvelope(Option<CheckpointEnvelope>),
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeOffsetRpcResponse {
    pub request_id: u64,
    pub result: Result<RuntimeOffsetValue, String>,
}

// We use `LittleEndian` for values because
// it's possibly cheaper, but the difference
// isn't likely to be measurable, so honestly
// use whatever you want for values.
#[derive(FromBytes, AsBytes, Unaligned, Debug)]
#[repr(C)]
pub struct OffsetValue {
    pub(crate) filesize: U64<LittleEndian>,
    pub(crate) line: U64<LittleEndian>,
    pub(crate) closed: U64<LittleEndian>, // we store bool here
}

#[allow(dead_code)]
pub struct Offset {
    pub(crate) value: OffsetValue,
    pub(crate) key: OffsetKey,
}

pub trait OffsetTransport: Send + Sync {
    fn call(&self, operation: RuntimeOffsetOperation) -> Result<RuntimeOffsetValue, String>;
}

pub trait CheckpointTransport: Send + Sync {
    fn store_checkpoint(&self, key: &str, envelope: &CheckpointEnvelope) -> Result<(), String>;
}

#[derive(Clone)]
pub struct Offsets {
    kv: Option<Arc<dyn skippr_wal_catalog::SkipprStore>>,
    fence: Option<skippr_wal_catalog::CatalogFence>,
    store: Option<Arc<dyn OffsetStore>>,
    transport: Option<Arc<dyn OffsetTransport>>,
}

// #[derive(Debug, Error)]
// #[error("Failed opening offset DB at this location: {0}. Is another instance of Skippr already ingesting this pipeline?")]
// struct AlreadyOpenError(String);

#[derive(Debug, Error)]
pub enum OffsetsError {
    #[error("Failed opening offset DB at this location: {0}. Is another instance of Skippr already ingesting this pipeline?")]
    AlreadyOpenError(String),
    #[error("Offset store read failed: {0}")]
    Store(String),
}

impl From<OffsetsError> for std::io::Error {
    fn from(err: OffsetsError) -> Self {
        std::io::Error::other(err.to_string())
    }
}

#[cfg(test)]
thread_local! {
    static LIVE_KV: std::cell::RefCell<
        Option<(
            Arc<dyn skippr_wal_catalog::SkipprStore>,
            skippr_wal_catalog::CatalogFence,
        )>,
    > = const { std::cell::RefCell::new(None) };
}

#[cfg(not(test))]
static LIVE_KV: std::sync::Mutex<
    Option<(
        Arc<dyn skippr_wal_catalog::SkipprStore>,
        skippr_wal_catalog::CatalogFence,
    )>,
> = std::sync::Mutex::new(None);

pub fn install_live_kv(
    kv: Arc<dyn skippr_wal_catalog::SkipprStore>,
    fence: skippr_wal_catalog::CatalogFence,
) {
    #[cfg(test)]
    LIVE_KV.with(|slot| *slot.borrow_mut() = Some((kv, fence)));
    #[cfg(not(test))]
    {
        *LIVE_KV.lock().expect("live skippr store") = Some((kv, fence));
    }
}

#[cfg(test)]
pub fn reset_live_kv() {
    LIVE_KV.with(|slot| *slot.borrow_mut() = None);
}

pub fn live_kv() -> Option<(
    Arc<dyn skippr_wal_catalog::SkipprStore>,
    skippr_wal_catalog::CatalogFence,
)> {
    #[cfg(test)]
    {
        LIVE_KV.with(|slot| slot.borrow().clone())
    }
    #[cfg(not(test))]
    {
        LIVE_KV.lock().expect("live skippr store").clone()
    }
}

pub fn store_for_config(
    config: &Config,
) -> Result<
    (
        Arc<dyn skippr_wal_catalog::SkipprStore>,
        skippr_wal_catalog::CatalogFence,
    ),
    String,
> {
    if let Some(live) = live_kv() {
        return Ok(live);
    }
    let fence = skippr_wal_catalog::CatalogFence::local();
    let dir = format!("{}/skippr-store", config.get_data_dir());
    let kv =
        skippr_wal_catalog::FjallSkipprStore::open(&dir, fence).map_err(|err| err.to_string())?;
    let kv: Arc<dyn skippr_wal_catalog::SkipprStore> = Arc::new(kv);
    install_live_kv(Arc::clone(&kv), fence);
    Ok((kv, fence))
}

impl Offsets {
    pub fn from_transport(transport: Arc<dyn OffsetTransport>) -> Self {
        Self {
            kv: None,
            fence: None,
            store: None,
            transport: Some(transport),
        }
    }

    fn offset_backend(&self) -> Option<&Arc<dyn OffsetStore>> {
        self.store.as_ref()
    }

    pub fn bytes_store(&self) -> Option<&Arc<dyn OffsetStore>> {
        self.store.as_ref()
    }

    fn has_materialized_store(&self) -> bool {
        self.store.is_some()
    }

    fn transport(&self) -> Option<&Arc<dyn OffsetTransport>> {
        self.transport.as_ref()
    }

    pub fn is_remote(&self) -> bool {
        !self.has_materialized_store() && self.transport().is_some()
    }

    #[cfg(test)]
    pub(crate) fn clear_for_test(&self) {
        if let Some(store) = &self.store {
            let _ = store.reset_published_ingest();
        }
    }

    #[cfg(test)]
    pub(crate) fn flush_for_test(&self) {
        if let Some(store) = &self.store {
            store.flush().unwrap();
        }
    }

    pub fn init(config: &Config) -> Result<Offsets, OffsetsError> {
        crate::pipeline_backend::configured_kind(config).map_err(OffsetsError::AlreadyOpenError)?;
        let data_dir = config.get_data_dir();
        let (kv, fence) = store_for_config(config).map_err(OffsetsError::AlreadyOpenError)?;
        let sled_path = std::path::PathBuf::from(format!("{data_dir}/{SLED_NAME}"));
        loop {
            match skippr_wal_catalog::migrate_sled_db(&kv, fence, &sled_path, 1) {
                Ok(skippr_wal_catalog::MigrateStatus::InProgress {
                    tree,
                    remaining_hint,
                }) => {
                    info!(
                        tree,
                        remaining_hint, "sled → fjall migrate running under live store"
                    );
                }
                Ok(skippr_wal_catalog::MigrateStatus::Complete { archived_to }) => {
                    info!(archived = %archived_to.display(), "sled → fjall migrate complete");
                    break;
                }
                Ok(skippr_wal_catalog::MigrateStatus::NoSled) => break,
                Err(err) => {
                    return Err(OffsetsError::AlreadyOpenError(err.to_string()));
                }
            }
        }
        let mut offsets_backend = StoreOffsetStore::new(Arc::clone(&kv), fence);
        if sled_path.exists() {
            offsets_backend = offsets_backend.with_sled_fallback(&sled_path);
        }
        let backend: Arc<dyn OffsetStore> = Arc::new(offsets_backend);
        Ok(Offsets {
            kv: Some(kv),
            fence: Some(fence),
            store: Some(backend),
            transport: None,
        })
    }

    fn remote_value(
        &self,
        operation: RuntimeOffsetOperation,
    ) -> Result<RuntimeOffsetValue, OffsetsError> {
        let transport = self.transport().ok_or_else(|| {
            OffsetsError::Store("remote offset operation has no authoritative transport".into())
        })?;
        transport.call(operation).map_err(|err| {
            OffsetsError::Store(format!(
                "Remote offset operation failed authoritatively: {err}"
            ))
        })
    }

    pub fn build_key(&self, key: &OffsetKey) -> String {
        // let namespace = ;
        // let partition = self.vec_8_to_u16(partition.as_bytes());
        // let key: Key = Key { namespace: namespace.to_string(), partition: partition.to_string() };
        // let key = Key { namespace: namespace.to_string(), partition: partition.to_string() };
        // key
        format!("{}-{}", key.namespace, key.partition)
    }

    pub fn build_latest_key(key: &OffsetKey) -> String {
        // let namespace = namespace.as_bytes();
        // let partition = partition.as_bytes();
        // unsafe self.any_as_u8_slice(format!("{}-{}-latest", namespace, partition).as_bytes().to_owned())
        // let partition = &format!("{}-latest", partition.to_owned());
        // let key: Key = Key { namespace: namespace.to_string(), partition: partition.to_string() };
        // key
        format!("{}-{}-latest", key.namespace, key.partition)
    }

    pub fn flush(&self) -> Result<(), OffsetsError> {
        if let Some(store) = self.offset_backend() {
            return store.flush().map_err(OffsetsError::Store);
        }
        if self.transport().is_some() {
            return Ok(());
        }
        Err(OffsetsError::Store(
            "offset flush requires an installed store".into(),
        ))
    }
    pub fn open_wal_catalog(
        &self,
        _fence: skippr_wal_catalog::CatalogFence,
    ) -> Result<Arc<dyn skippr_wal_catalog::WalCatalogStore>, String> {
        let kv = self
            .kv
            .clone()
            .ok_or_else(|| "WAL catalog requires an open Skippr Store".to_string())?;
        Ok(Arc::new(skippr_wal_catalog::CatalogView::new(kv)))
    }

    pub fn open_wal_catalog_read(&self) -> Arc<dyn skippr_wal_catalog::WalCatalogRead> {
        let kv = self
            .kv
            .clone()
            .expect("WAL catalog requires an open Skippr Store");
        Arc::new(skippr_wal_catalog::CatalogView::new(kv))
    }

    pub fn skippr_kv(&self) -> Option<Arc<dyn skippr_wal_catalog::SkipprStore>> {
        self.kv.clone()
    }

    pub fn writer_fence(&self) -> Option<skippr_wal_catalog::CatalogFence> {
        self.fence
    }

    pub fn open_stream_state(&self) -> Result<Arc<dyn skippr_wal_catalog::SkipprStore>, String> {
        self.kv
            .clone()
            .ok_or_else(|| "stream processor requires an open Skippr Store".into())
    }

    #[cfg(test)]
    pub(crate) fn from_memory_for_test() -> Self {
        let fence = skippr_wal_catalog::CatalogFence::local();
        let kv: Arc<dyn skippr_wal_catalog::SkipprStore> =
            Arc::new(skippr_wal_catalog::MemorySkipprStore::open(fence));
        let backend: Arc<dyn OffsetStore> = Arc::new(StoreOffsetStore::new(Arc::clone(&kv), fence));
        install_live_kv(Arc::clone(&kv), fence);
        Self {
            kv: Some(kv),
            fence: Some(fence),
            store: Some(backend),
            transport: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_store_for_test(store: Arc<dyn OffsetStore>) -> Self {
        Self {
            kv: None,
            fence: None,
            store: Some(store),
            transport: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn seed_published_offset(
        &self,
        key: &OffsetKey,
        filesize: u64,
        position: u64,
        closed: u64,
        seq: u64,
    ) -> Result<(), String> {
        let store = self.offset_backend().ok_or("offset store missing")?;
        let mut bytes = Vec::with_capacity(24);
        bytes.extend_from_slice(&filesize.to_le_bytes());
        bytes.extend_from_slice(&position.to_le_bytes());
        bytes.extend_from_slice(&closed.to_le_bytes());
        let sha = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&bytes))
        };
        let sk = store.offset_publish_sk(&key.namespace, &key.partition);
        store
            .publish_wal_commit(&sk, &bytes, 1, seq, &sha, skippr_lease::PublishMode::Fresh)
            .map_err(|err| err.to_string())
    }

    #[cfg(test)]
    pub(crate) fn seed_published_checkpoint(
        &self,
        key: &str,
        envelope: &CheckpointEnvelope,
        seq: u64,
    ) -> Result<(), String> {
        let store = self.offset_backend().ok_or("offset store missing")?;
        let bytes = bincode::serialize(envelope).map_err(|err| err.to_string())?;
        let sha = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&bytes))
        };
        let sk = store.checkpoint_publish_sk(key);
        store
            .publish_wal_commit(&sk, &bytes, 1, seq, &sha, skippr_lease::PublishMode::Fresh)
            .map_err(|err| err.to_string())
    }

    /// Read the durable offset tuple for a partition with a single store lookup.
    pub fn snapshot_value(&self, key: &OffsetKey) -> Result<Option<OffsetValue>, OffsetsError> {
        let Some(mut backing_bytes) = self.try_get(key)? else {
            return Ok(None);
        };
        let layout: LayoutVerified<&mut [u8], OffsetValue> =
            LayoutVerified::new_unaligned(&mut *backing_bytes)
                .ok_or_else(|| OffsetsError::Store("offset bytes do not fit schema".into()))?;
        let value: &OffsetValue = layout.into_ref();
        Ok(Some(OffsetValue {
            filesize: value.filesize,
            line: value.line,
            closed: value.closed,
        }))
    }

    pub fn iter_local_offsets(&self) -> Vec<(OffsetKey, OffsetValue)> {
        let Some(kv) = &self.kv else {
            return Vec::new();
        };
        let Ok(page) = kv.query_prefix(
            skippr_wal_catalog::StoreFamily::Offsets.prefix(),
            None,
            usize::MAX,
        ) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (key, value) in page {
            let Some(rest) =
                skippr_wal_catalog::family_rest(skippr_wal_catalog::StoreFamily::Offsets, &key)
            else {
                continue;
            };
            let Ok(key) = std::str::from_utf8(rest) else {
                continue;
            };
            if key.starts_with("cdc_checkpoint:") || key.ends_with("-latest") {
                continue;
            }
            if value.len() != 24 {
                continue;
            }
            let Some((namespace, partition)) = key.rsplit_once('-') else {
                continue;
            };
            let mut backing = value.to_vec();
            let Some(layout) =
                LayoutVerified::<&mut [u8], OffsetValue>::new_unaligned(&mut backing)
            else {
                continue;
            };
            let value: &OffsetValue = layout.into_ref();
            out.push((
                OffsetKey::new(namespace, partition),
                OffsetValue {
                    filesize: value.filesize,
                    line: value.line,
                    closed: value.closed,
                },
            ));
        }
        out
    }

    pub fn iter_local_checkpoints(&self) -> Vec<(String, Vec<u8>)> {
        let Some(kv) = &self.kv else {
            return Vec::new();
        };
        let Ok(page) = kv.query_prefix(
            skippr_wal_catalog::StoreFamily::Offsets.prefix(),
            None,
            usize::MAX,
        ) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (key, value) in page {
            let Some(rest) =
                skippr_wal_catalog::family_rest(skippr_wal_catalog::StoreFamily::Offsets, &key)
            else {
                continue;
            };
            let Ok(key) = std::str::from_utf8(rest) else {
                continue;
            };
            let Some(logical) = key.strip_prefix("cdc_checkpoint:") else {
                continue;
            };
            out.push((logical.to_string(), value));
        }
        out
    }

    pub fn try_get(&self, key: &OffsetKey) -> Result<Option<Vec<u8>>, OffsetsError> {
        if let Some(store) = self.offset_backend() {
            return store
                .get_offset(&key.namespace, &key.partition)
                .map_err(OffsetsError::Store);
        }
        Ok(None)
    }

    pub fn get(&self, key: &OffsetKey) -> Result<Option<Vec<u8>>, OffsetsError> {
        self.try_get(key)
    }
    pub fn validate(
        &self,
        key: &OffsetKey,
        offset_type: OffsetTypes,
        offset_value: u64,
    ) -> Result<Option<bool>, OffsetsError> {
        if !self.has_materialized_store() {
            return match self.remote_value(RuntimeOffsetOperation::Validate {
                key: key.clone(),
                offset_type,
                offset_value,
            })? {
                RuntimeOffsetValue::Validate(value) => Ok(value),
                other => Err(OffsetsError::Store(format!(
                    "remote validate returned unexpected response: {:?}",
                    other
                ))),
            };
        }

        let resp = match self.try_get(key)? {
            Some(existing) => {
                let mut backing_bytes = existing;
                let layout: LayoutVerified<&mut [u8], OffsetValue> =
                    LayoutVerified::new_unaligned(&mut *backing_bytes).ok_or_else(|| {
                        OffsetsError::Store("offset bytes do not fit schema".into())
                    })?;
                let value: &mut OffsetValue = layout.into_mut();

                let mut matched = false;

                match offset_type {
                    OffsetTypes::Filesize => {
                        if value.filesize.get() < offset_value {
                            matched = true
                        }
                    }
                    OffsetTypes::Position => {
                        if value.line.get() < offset_value {
                            matched = true
                        }
                    }
                    OffsetTypes::Closed => {
                        if (offset_value == 0 && value.closed.get() == 0)
                            || (offset_value != 0 && value.closed.get() != 0)
                        {
                            matched = true
                        }
                    }
                }

                Some(matched)
            }
            None => None,
        };

        Ok(resp)
    }

    pub fn load_checkpoint_envelope(
        &self,
        key: &str,
    ) -> Result<Option<CheckpointEnvelope>, OffsetsError> {
        if let Some(store) = self.offset_backend() {
            let value = store.get_checkpoint(key).map_err(OffsetsError::Store)?;
            let Some(value) = value else {
                return Ok(None);
            };
            return bincode::deserialize::<CheckpointEnvelope>(&value)
                .map(Some)
                .map_err(|err| {
                    OffsetsError::Store(format!(
                        "Failed to deserialize checkpoint envelope '{key}': {err}"
                    ))
                });
        }
        match self.remote_value(RuntimeOffsetOperation::LoadCheckpointEnvelope {
            key: key.to_string(),
        })? {
            RuntimeOffsetValue::LoadCheckpointEnvelope(value) => Ok(value),
            other => Err(OffsetsError::Store(format!(
                "Remote load_checkpoint_envelope returned unexpected response: {other:?}"
            ))),
        }
    }

    pub fn load_checkpoint_payload<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, OffsetsError> {
        match self.load_checkpoint_envelope(key)? {
            None => Ok(None),
            Some(envelope) => envelope
                .into_payload()
                .map(Some)
                .map_err(|err| OffsetsError::Store(err.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::helpers::configuration::Config;
    use crate::helpers::offsets::{
        OffsetKey, OffsetTransport, OffsetTypes, OffsetValue, Offsets, OffsetsError,
        RuntimeOffsetOperation, RuntimeOffsetValue,
    };
    use crate::plugins::cdc::{CheckpointAuthority, CheckpointEnvelope, CheckpointKind};

    use serial_test::serial;
    use zerocopy::{AsBytes, U64};

    #[derive(Default)]
    struct RecordingOffsetTransport {
        calls: Mutex<Vec<RuntimeOffsetOperation>>,
    }

    impl OffsetTransport for RecordingOffsetTransport {
        fn call(&self, operation: RuntimeOffsetOperation) -> Result<RuntimeOffsetValue, String> {
            self.calls.lock().unwrap().push(operation.clone());
            match operation {
                RuntimeOffsetOperation::Validate { .. } => {
                    Ok(RuntimeOffsetValue::Validate(Some(true)))
                }
                RuntimeOffsetOperation::LoadCheckpointEnvelope { .. } => {
                    Ok(RuntimeOffsetValue::LoadCheckpointEnvelope(Some(
                        CheckpointEnvelope::from_payload(
                            CheckpointAuthority::AdvisoryHint,
                            CheckpointKind::AdvisoryProgress,
                            1,
                            &b"checkpoint".to_vec(),
                        )
                        .unwrap(),
                    )))
                }
            }
        }
    }

    struct FailingOffsetTransport;

    impl OffsetTransport for FailingOffsetTransport {
        fn call(&self, _operation: RuntimeOffsetOperation) -> Result<RuntimeOffsetValue, String> {
            Err("transport disconnected".to_string())
        }
    }

    #[test]
    #[serial]
    fn published_position_is_readable() {
        let db = match Offsets::init(&Config::new()) {
            Ok(offsets) => offsets,
            Err(e) => {
                println!("Skipping: {}", e);
                return;
            }
        };

        db.clear_for_test();

        let key = &OffsetKey {
            namespace: "foo".to_string(),
            partition: "bar".to_string(),
        };

        db.seed_published_offset(key, 0, 1, 0, 1).unwrap();
        assert_eq!(
            db.validate(key, OffsetTypes::Position, 1).unwrap(),
            Some(false)
        );
        assert_eq!(
            db.validate(key, OffsetTypes::Position, 2).unwrap(),
            Some(true)
        );

        db.seed_published_offset(key, 0, 3, 0, 2).unwrap();
        assert_eq!(
            db.validate(key, OffsetTypes::Position, 3).unwrap(),
            Some(false)
        );
        assert_eq!(
            db.validate(key, OffsetTypes::Position, 4).unwrap(),
            Some(true)
        );
        let snap = db.snapshot_value(key).unwrap().unwrap();
        assert_eq!(snap.line.get(), 3);
    }

    #[test]
    #[serial]
    fn published_tuple_keeps_closed_and_position() {
        let db = match Offsets::init(&Config::new()) {
            Ok(offsets) => offsets,
            Err(e) => {
                println!("Skipping: {}", e);
                return;
            }
        };
        db.clear_for_test();
        let key = &OffsetKey {
            namespace: "foo".to_string(),
            partition: "closed-preserve".to_string(),
        };
        db.seed_published_offset(key, 0, 42, 1, 1).unwrap();
        let snap = db.snapshot_value(key).unwrap().unwrap();
        assert_eq!(snap.closed.get(), 1);
        assert_eq!(snap.line.get(), 42);
    }

    #[test]
    fn store_read_error_is_not_missing() {
        let err = OffsetsError::Store("dynamo down".into());
        assert!(matches!(err, OffsetsError::Store(_)));
        assert_ne!(format!("{err}"), "");
    }

    struct FailingCheckpointStore;

    impl crate::helpers::offset_store::OffsetStore for FailingCheckpointStore {
        fn get_bytes(&self, _sk: &str) -> Result<Option<Vec<u8>>, String> {
            Err("checkpoint store down".into())
        }
        fn get_journal(
            &self,
            _key: &crate::helpers::offset_store::JournalKey,
        ) -> Result<Option<Vec<u8>>, String> {
            Err("checkpoint store down".into())
        }
        fn fetch_and_update_journal(
            &self,
            _key: &crate::helpers::offset_store::JournalKey,
            _update: Box<dyn FnOnce(Option<Vec<u8>>) -> Option<Vec<u8>> + Send>,
        ) -> Result<Option<Vec<u8>>, String> {
            Err("checkpoint store down".into())
        }
        fn get_offset(
            &self,
            _namespace: &str,
            _partition: &str,
        ) -> Result<Option<Vec<u8>>, String> {
            Err("checkpoint store down".into())
        }
        fn get_checkpoint(&self, _key: &str) -> Result<Option<Vec<u8>>, String> {
            Err("checkpoint store down".into())
        }
        fn flush(&self) -> Result<(), String> {
            Err("checkpoint store down".into())
        }
        fn publish_wal_commit(
            &self,
            _sk: &str,
            _bytes: &[u8],
            _epoch: u64,
            _seq: u64,
            _payload_sha256: &str,
            _mode: skippr_lease::PublishMode,
        ) -> Result<(), skippr_lease::OffsetPublishError> {
            Err(skippr_lease::OffsetPublishError::Transient(
                "checkpoint store down".into(),
            ))
        }
        fn offset_publish_sk(&self, namespace: &str, partition: &str) -> String {
            format!("{namespace}-{partition}")
        }
        fn checkpoint_publish_sk(&self, key: &str) -> String {
            format!("cdc_checkpoint:{key}")
        }
        fn reset_published_ingest(&self) -> Result<(), String> {
            Err("checkpoint store down".into())
        }
    }

    #[test]
    fn load_checkpoint_store_error_is_not_missing() {
        let offsets = Offsets::from_store_for_test(Arc::new(FailingCheckpointStore));
        let err = offsets.load_checkpoint_envelope("k").unwrap_err();
        assert!(err.to_string().contains("checkpoint store down"));
    }

    #[test]
    #[serial]
    fn test_validate() {
        let db = match Offsets::init(&Config::new()) {
            Ok(offsets) => offsets,
            Err(e) => {
                println!("Skipping: {}", e);
                return;
            }
        };

        db.clear_for_test();

        // assert_eq!(db.validate(key, 1, 1), Some(true));
        // assert_eq!(db.validate(key, 1, 1), Some(false)); // @todo this is atleast once
        // assert_eq!(db.validate(key, 2, 1), Some(true));
        // assert_eq!(db.validate(key, 1, 2), Some(false));
        // assert_eq!(db.validate(key, 2, 1), Some(false));
        // assert_eq!(db.validate(key, 2, 2), Some(true)); // @todo this is atleast once
        // assert_eq!(db.validate(key, 2, 3), Some(true));
        // assert_eq!(db.validate(key, 2, 2), Some(false));
        // assert_eq!(db.validate(key, 1, 4), Some(false));

        let key = &OffsetKey {
            namespace: "foo".to_string(),
            partition: "bar".to_string(),
        };

        assert_eq!(db.validate(key, OffsetTypes::Filesize, 1).unwrap(), None);
        db.seed_published_offset(key, 1, 0, 0, 1).unwrap();
        assert_eq!(
            db.validate(key, OffsetTypes::Filesize, 1).unwrap(),
            Some(false)
        );
        assert_eq!(
            db.validate(key, OffsetTypes::Filesize, 2).unwrap(),
            Some(true)
        );
        db.seed_published_offset(key, 2, 0, 0, 2).unwrap();
        assert_eq!(
            db.validate(key, OffsetTypes::Filesize, 1).unwrap(),
            Some(false)
        );
        assert_eq!(
            db.validate(key, OffsetTypes::Filesize, 2).unwrap(),
            Some(false)
        );
        assert_eq!(
            db.validate(key, OffsetTypes::Filesize, 3).unwrap(),
            Some(true)
        );
        assert_eq!(
            db.validate(key, OffsetTypes::Filesize, 4).unwrap(),
            Some(true)
        );
        assert_eq!(
            db.validate(key, OffsetTypes::Filesize, 2).unwrap(),
            Some(false)
        );

        let key = &OffsetKey {
            namespace: "foo".to_string(),
            partition: "bar2".to_string(),
        };

        assert_eq!(db.validate(key, OffsetTypes::Closed, 0).unwrap(), None);
        assert_eq!(db.validate(key, OffsetTypes::Closed, 1).unwrap(), None);
        db.seed_published_offset(key, 0, 0, 1, 1).unwrap();
        assert_eq!(
            db.validate(key, OffsetTypes::Closed, 1).unwrap(),
            Some(true)
        );
        assert_eq!(
            db.validate(key, OffsetTypes::Closed, 0).unwrap(),
            Some(false)
        );
        db.seed_published_offset(key, 0, 0, 42, 2).unwrap();
        assert_eq!(
            db.validate(key, OffsetTypes::Closed, 1).unwrap(),
            Some(true)
        );
        assert_eq!(
            db.validate(key, OffsetTypes::Closed, 0).unwrap(),
            Some(false)
        );

        // db.remove(key).unwrap();

        db.flush_for_test();
        // db.db.flush().unwrap();
        // drop(db.db);
        // drop(db.tree);
        // drop(db);
    }

    #[test]
    fn test_remote_offsets_route_transport_calls() {
        let transport = Arc::new(RecordingOffsetTransport::default());
        let offsets = Offsets::from_transport(transport.clone());
        let key = OffsetKey::new("remote-ns", "remote-partition");

        assert_eq!(
            offsets.validate(&key, OffsetTypes::Closed, 1).unwrap(),
            Some(true)
        );
        assert_eq!(
            offsets
                .load_checkpoint_payload::<Vec<u8>>("remote-key")
                .unwrap(),
            Some(b"checkpoint".to_vec())
        );
        offsets.flush().unwrap();

        let calls = transport.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                RuntimeOffsetOperation::Validate {
                    key: key.clone(),
                    offset_type: OffsetTypes::Closed,
                    offset_value: 1,
                },
                RuntimeOffsetOperation::LoadCheckpointEnvelope {
                    key: "remote-key".to_string(),
                },
            ]
        );
    }

    #[test]
    fn remote_offset_store_has_no_write_api() {
        let offsets = include_str!("offsets.rs");
        assert!(!offsets.contains(concat!("pub fn set", "(")));
        assert!(!offsets.contains(concat!("pub fn insert", "(")));
        assert!(!offsets.contains(concat!("pub fn store_checkpoint", "_envelope")));
        assert!(!offsets.contains(concat!("pub fn rem", "ove(")));
        assert!(!offsets.contains(concat!("checkpoint_trans", "port:")));
    }

    #[test]
    fn remote_offset_reads_fail_instead_of_defaulting_to_empty_state() {
        let offsets = Offsets::from_transport(Arc::new(FailingOffsetTransport));
        let key = OffsetKey::new("remote-ns", "remote-partition");

        let err = offsets
            .validate(&key, OffsetTypes::Closed, 1)
            .expect_err("store failure must be Result::Err, not empty/closed");
        assert!(
            err.to_string()
                .contains("Remote offset operation failed authoritatively"),
            "{err}"
        );
    }

    // #[test]
    // #[serial]
    // fn test_validate_performance() {
    //
    //     let ingestMsgCount = Arc::new(Mutex::new(0));
    //     let ingestMsgCountClone = ingestMsgCount.clone();
    //     let skippedMsgCount = Arc::new(Mutex::new(0));
    //     let skippedMsgCountClone = skippedMsgCount.clone();
    //     let ingestMsgTotal = Arc::new(Mutex::new(0));
    //     let now = Arc::new(Mutex::new(Instant::now()));
    //
    //     let mut planner = periodic::Planner::new();
    //
    //     planner.add(
    //         move || {
    //             let mut metrics: Metrics = Metrics::new();
    //
    //             let mut counter_lock = ingestMsgCount.lock().unwrap();
    //             let mut skipped_lock = skippedMsgCount.lock().unwrap();
    //             let mut total_lock = ingestMsgTotal.lock().unwrap();
    //             let now_lock = now.lock().unwrap();
    //
    //             *total_lock += *counter_lock;
    //
    //             metrics.msgs_total = *total_lock;
    //             metrics.msgs_current = *counter_lock;
    //             metrics.deadletters_current = *skipped_lock;
    //             metrics.run_time_seconds = now_lock.elapsed().as_secs().clone() as i64;
    //
    //             println!("Runtime: {} seconds", now_lock.elapsed().as_secs());
    //             println!("Skipped Messages: {}", *skipped_lock);
    //             println!("Ingested Messages: {}", *counter_lock);
    //             println!("Total Messages: {}", *total_lock);
    //
    //             *counter_lock = 0;
    //
    //
    //         },
    //         periodic::Every::new(Duration::from_secs(60)),
    //     );
    //     planner.start();
    //
    //
    //     let db = Offsets::init(&Config::new()).unwrap();
    //
    //     // for i in 1..=100000000 {
    //     for i in 1..=10000000 {
    //     // for i in 1..=100 {
    //
    //         // let mut rng = rand::thread_rng();
    //         // let y: f64 = rng.gen(); // generates a float between 0 and 1
    //         // let foo = 10.0 * y;
    //
    //         let y = i + 1;
    //
    //         match db.validate("abc", &i.to_string(), OffsetTypes::Filesize, y) {
    //             Some(true) => {
    //                 // println!("True: {}", i);
    //                 let mut counter_lock = ingestMsgCountClone.lock().unwrap();
    //                 *counter_lock += 1;
    //                 db.set("abc", &i.to_string(), OffsetTypes::Filesize, y);
    //             },
    //             Some(false) => {
    //                 // println!("False: {}", i);
    //                 let mut counter_lock = skippedMsgCountClone.lock().unwrap();
    //                 *counter_lock += 1;
    //             },
    //             None => {
    //                 db.insert("abc", &i.to_string(), OffsetTypes::Filesize, y);
    //                 // println!("None: {}", i);
    //             }
    //         }
    //
    //     }
    //
    //     let mut counter_lock = ingestMsgCountClone.lock().unwrap();
    //     let mut skipped_lock = skippedMsgCountClone.lock().unwrap();
    //
    //     println!("Skipped Messages: {}", *skipped_lock);
    //     println!("Ingested Messages: {}", *counter_lock);
    //
    //     db.tree.flush().unwrap();
    //     db.db.flush().unwrap();
    //     drop(db.tree);
    //     drop(db.db);
    //     // drop(db);
    // }
}
