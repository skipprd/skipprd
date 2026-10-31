use std::collections::BTreeMap;
use std::sync::Mutex;

use skippr_lease::{LeaseEpoch, NodeId};

use crate::apply::{eval_ops, Applied};
use crate::error::CatalogError;
use crate::key::{CatalogKey, CatalogPrefix};
use crate::row::{CatalogFence, CatalogRow};
use crate::store::{WalCatalogRead, WalCatalogStore};
use crate::txn::{CatalogTxn, TXN_MAX_OPS};

struct LeaseSlot {
    owner: NodeId,
    epoch: LeaseEpoch,
    released: bool,
}

struct Inner {
    rows: BTreeMap<String, Vec<u8>>,
    lease: Option<LeaseSlot>,
}

pub struct MemoryWalCatalog {
    inner: Mutex<Inner>,
}

impl MemoryWalCatalog {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                rows: BTreeMap::new(),
                lease: None,
            }),
        }
    }

    pub fn set_writer_fence(&self, owner: NodeId, epoch: LeaseEpoch) {
        let mut inner = self.inner.lock().expect("memory catalog poisoned");
        inner.lease = Some(LeaseSlot {
            owner,
            epoch,
            released: false,
        });
    }

    pub fn revoke_writer_fence(&self) {
        let mut inner = self.inner.lock().expect("memory catalog poisoned");
        if let Some(slot) = inner.lease.as_mut() {
            slot.released = true;
        }
    }

    /// Fixture format-2 leftover rows. Production commits reject `LegacySlice` via `validate_put`.
    pub fn seed_historical(&self, key: CatalogKey, row: CatalogRow) -> Result<(), CatalogError> {
        let bytes = row.encode()?;
        let mut inner = self.inner.lock().expect("memory catalog poisoned");
        inner.rows.insert(key.encode(), bytes);
        Ok(())
    }

    fn check_fence(inner: &Inner, fence: CatalogFence) -> Result<(), CatalogError> {
        match &inner.lease {
            Some(lease)
                if lease.owner == fence.owner && lease.epoch == fence.epoch && !lease.released =>
            {
                Ok(())
            }
            _ => Err(CatalogError::Fenced),
        }
    }
}

impl Default for MemoryWalCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl WalCatalogRead for MemoryWalCatalog {
    fn get(&self, key: &CatalogKey) -> Result<Option<CatalogRow>, CatalogError> {
        let inner = self.inner.lock().expect("memory catalog poisoned");
        inner
            .rows
            .get(&key.encode())
            .map(|bytes| CatalogRow::decode(bytes))
            .transpose()
    }

    fn query_prefix(
        &self,
        prefix: CatalogPrefix,
        after: Option<&CatalogKey>,
        limit: usize,
    ) -> Result<Vec<(CatalogKey, CatalogRow)>, CatalogError> {
        let inner = self.inner.lock().expect("memory catalog poisoned");
        let start = after.map(CatalogKey::encode);
        let p = prefix.as_str();
        let mut out = Vec::new();
        for (sk, bytes) in inner.rows.range(p.to_string()..) {
            if !sk.starts_with(p) {
                break;
            }
            if let Some(start) = start.as_ref() {
                if sk.as_str() <= start.as_str() {
                    continue;
                }
            }
            let key = CatalogKey::decode(sk).map_err(|err| CatalogError::Codec(err.to_string()))?;
            out.push((key, CatalogRow::decode(bytes)?));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }
}

impl WalCatalogStore for MemoryWalCatalog {
    fn commit(&self, txn: &CatalogTxn, fence: CatalogFence) -> Result<(), CatalogError> {
        let mut inner = self.inner.lock().expect("memory catalog poisoned");
        Self::check_fence(&inner, fence)?;
        let planned = eval_ops(txn, |key| Ok(inner.rows.get(&key.encode()).cloned()))?;
        for applied in planned {
            match applied {
                Applied::Put { key, bytes } => {
                    inner.rows.insert(key.encode(), bytes);
                }
                Applied::Delete { key } => {
                    inner.rows.remove(&key.encode());
                }
            }
        }
        Ok(())
    }

    fn delete_prefix(
        &self,
        prefix: CatalogPrefix,
        fence: CatalogFence,
    ) -> Result<(), CatalogError> {
        loop {
            let page = self.query_prefix(prefix, None, TXN_MAX_OPS)?;
            if page.is_empty() {
                return Ok(());
            }
            let mut txn = CatalogTxn::new();
            for (key, _) in page {
                txn.delete(key, crate::row::DeleteCond::Exists)?;
            }
            self.commit(&txn, fence)?;
        }
    }

    fn flush_durable(&self) -> Result<(), CatalogError> {
        Ok(())
    }
}
