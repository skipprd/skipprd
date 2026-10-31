use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use skippr_lease::{LeaseEpoch, NodeId};

use crate::apply::{eval_ops, Applied};
use crate::error::CatalogError;
use crate::key::{CatalogKey, CatalogPrefix};
use crate::kv::{KvOp, SkipprStore, SkipprStoreRead, StoreFamily};
use crate::row::{CatalogFence, CatalogRow};
use crate::store::{WalCatalogRead, WalCatalogStore};
use crate::txn::{CatalogTxn, TXN_MAX_OPS};

struct Inner {
    rows: BTreeMap<Vec<u8>, Vec<u8>>,
    fence: Option<CatalogFence>,
    released: bool,
}

#[derive(Clone)]
pub struct MemorySkipprStore {
    inner: Arc<Mutex<Inner>>,
}

/// DST/catalog name for the memory Skippr Store (also the WAL catalog).
pub type MemoryWalCatalog = MemorySkipprStore;

impl MemorySkipprStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                rows: BTreeMap::new(),
                fence: None,
                released: false,
            })),
        }
    }

    pub fn open(fence: CatalogFence) -> Self {
        let store = Self::new();
        store.set_writer_fence(fence.owner, fence.epoch);
        store
    }

    pub fn set_writer_fence(&self, owner: NodeId, epoch: LeaseEpoch) {
        let mut inner = self.lock();
        inner.fence = Some(CatalogFence { owner, epoch });
        inner.released = false;
    }

    pub fn revoke_writer_fence(&self) {
        self.lock().released = true;
    }

    pub fn seed_historical(&self, key: CatalogKey, row: CatalogRow) -> Result<(), CatalogError> {
        let bytes = row.encode()?;
        self.lock()
            .rows
            .insert(StoreFamily::Catalog.key(key.encode().as_bytes()), bytes);
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("memory skippr store poisoned")
    }

    fn check_fence(inner: &Inner, fence: CatalogFence) -> Result<(), CatalogError> {
        match inner.fence {
            Some(owned)
                if owned.owner == fence.owner && owned.epoch == fence.epoch && !inner.released =>
            {
                Ok(())
            }
            _ => Err(CatalogError::Fenced),
        }
    }
}

impl Default for MemorySkipprStore {
    fn default() -> Self {
        Self::new()
    }
}

struct MemoryPin {
    rows: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl SkipprStoreRead for MemoryPin {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, CatalogError> {
        Ok(self.rows.get(key).cloned())
    }

    fn query_prefix(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CatalogError> {
        query_map(&self.rows, prefix, after, limit)
    }
}

impl SkipprStoreRead for MemorySkipprStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, CatalogError> {
        Ok(self.lock().rows.get(key).cloned())
    }

    fn query_prefix(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CatalogError> {
        query_map(&self.lock().rows, prefix, after, limit)
    }
}

impl SkipprStore for MemorySkipprStore {
    fn commit(&self, ops: &[KvOp], fence: CatalogFence) -> Result<(), CatalogError> {
        let mut inner = self.lock();
        Self::check_fence(&inner, fence)?;
        for op in ops {
            match op {
                KvOp::Put { key, value } => {
                    inner.rows.insert(key.clone(), value.clone());
                }
                KvOp::Delete { key } => {
                    inner.rows.remove(key);
                }
            }
        }
        Ok(())
    }

    fn flush_durable(&self) -> Result<(), CatalogError> {
        Ok(())
    }

    fn pin(&self) -> Result<Arc<dyn SkipprStoreRead>, CatalogError> {
        Ok(Arc::new(MemoryPin {
            rows: self.lock().rows.clone(),
        }))
    }
}

fn query_map(
    rows: &BTreeMap<Vec<u8>, Vec<u8>>,
    prefix: &[u8],
    after: Option<&[u8]>,
    limit: usize,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CatalogError> {
    let mut out = Vec::new();
    for (key, value) in rows.range(prefix.to_vec()..) {
        if !key.starts_with(prefix) {
            break;
        }
        if let Some(after) = after {
            if key.as_slice() <= after {
                continue;
            }
        }
        out.push((key.clone(), value.clone()));
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl WalCatalogRead for MemorySkipprStore {
    fn get(&self, key: &CatalogKey) -> Result<Option<CatalogRow>, CatalogError> {
        catalog_get(self, key)
    }

    fn query_prefix(
        &self,
        prefix: CatalogPrefix,
        after: Option<&CatalogKey>,
        limit: usize,
    ) -> Result<Vec<(CatalogKey, CatalogRow)>, CatalogError> {
        catalog_query_prefix(self, prefix, after, limit)
    }
}

impl WalCatalogStore for MemorySkipprStore {
    fn commit(&self, txn: &CatalogTxn, fence: CatalogFence) -> Result<(), CatalogError> {
        catalog_commit(self, txn, fence)
    }

    fn delete_prefix(
        &self,
        prefix: CatalogPrefix,
        fence: CatalogFence,
    ) -> Result<(), CatalogError> {
        catalog_delete_prefix(self, prefix, fence)
    }

    fn flush_durable(&self) -> Result<(), CatalogError> {
        SkipprStore::flush_durable(self)
    }
}

pub fn catalog_get(
    store: &dyn SkipprStoreRead,
    key: &CatalogKey,
) -> Result<Option<CatalogRow>, CatalogError> {
    let bytes = store.get(&StoreFamily::Catalog.key(key.encode().as_bytes()))?;
    bytes.map(|v| CatalogRow::decode(&v)).transpose()
}

pub fn catalog_query_prefix(
    store: &dyn SkipprStoreRead,
    prefix: CatalogPrefix,
    after: Option<&CatalogKey>,
    limit: usize,
) -> Result<Vec<(CatalogKey, CatalogRow)>, CatalogError> {
    let encoded_prefix = StoreFamily::Catalog.key(prefix.as_str().as_bytes());
    let encoded_after = after.map(|key| StoreFamily::Catalog.key(key.encode().as_bytes()));
    let page = store.query_prefix(&encoded_prefix, encoded_after.as_deref(), limit)?;
    let mut out = Vec::with_capacity(page.len());
    for (key, bytes) in page {
        let rest = crate::kv::family_rest(StoreFamily::Catalog, &key)
            .ok_or_else(|| CatalogError::Codec("catalog key missing family prefix".into()))?;
        let sk = std::str::from_utf8(rest).map_err(|err| CatalogError::Codec(err.to_string()))?;
        let catalog_key =
            CatalogKey::decode(sk).map_err(|err| CatalogError::Codec(err.to_string()))?;
        out.push((catalog_key, CatalogRow::decode(&bytes)?));
    }
    Ok(out)
}

pub fn catalog_commit(
    store: &dyn SkipprStore,
    txn: &CatalogTxn,
    fence: CatalogFence,
) -> Result<(), CatalogError> {
    let planned = eval_ops(txn, |key| {
        store.get(&StoreFamily::Catalog.key(key.encode().as_bytes()))
    })?;
    let ops: Vec<KvOp> = planned
        .into_iter()
        .map(|applied| match applied {
            Applied::Put { key, bytes } => KvOp::Put {
                key: StoreFamily::Catalog.key(key.encode().as_bytes()),
                value: bytes,
            },
            Applied::Delete { key } => KvOp::Delete {
                key: StoreFamily::Catalog.key(key.encode().as_bytes()),
            },
        })
        .collect();
    SkipprStore::commit(store, &ops, fence)
}

#[derive(Clone)]
pub struct CatalogView {
    store: Arc<dyn SkipprStore>,
}

impl CatalogView {
    pub fn new(store: Arc<dyn SkipprStore>) -> Self {
        Self { store }
    }
}

impl WalCatalogRead for CatalogView {
    fn get(&self, key: &CatalogKey) -> Result<Option<CatalogRow>, CatalogError> {
        catalog_get(self.store.as_ref(), key)
    }

    fn query_prefix(
        &self,
        prefix: CatalogPrefix,
        after: Option<&CatalogKey>,
        limit: usize,
    ) -> Result<Vec<(CatalogKey, CatalogRow)>, CatalogError> {
        catalog_query_prefix(self.store.as_ref(), prefix, after, limit)
    }
}

impl WalCatalogStore for CatalogView {
    fn commit(&self, txn: &CatalogTxn, fence: CatalogFence) -> Result<(), CatalogError> {
        catalog_commit(self.store.as_ref(), txn, fence)
    }

    fn delete_prefix(
        &self,
        prefix: CatalogPrefix,
        fence: CatalogFence,
    ) -> Result<(), CatalogError> {
        catalog_delete_prefix(self.store.as_ref(), prefix, fence)
    }

    fn flush_durable(&self) -> Result<(), CatalogError> {
        self.store.flush_durable()
    }
}

pub fn catalog_delete_prefix(
    store: &dyn SkipprStore,
    prefix: CatalogPrefix,
    fence: CatalogFence,
) -> Result<(), CatalogError> {
    loop {
        let page = catalog_query_prefix(store, prefix, None, TXN_MAX_OPS)?;
        if page.is_empty() {
            return Ok(());
        }
        let mut txn = CatalogTxn::new();
        for (key, _) in page {
            txn.delete(key, crate::row::DeleteCond::Exists)?;
        }
        catalog_commit(store, &txn, fence)?;
    }
}
