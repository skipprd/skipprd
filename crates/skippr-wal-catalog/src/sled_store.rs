use std::sync::Mutex;

use crate::apply::{eval_ops, Applied};
use crate::error::CatalogError;
use crate::key::{CatalogKey, CatalogPrefix};
use crate::row::{CatalogFence, CatalogRow};
use crate::store::{WalCatalogRead, WalCatalogStore};
use crate::txn::{CatalogTxn, TXN_MAX_OPS};

/// Local Skippr Store WAL catalog. Concurrent ingest + query on the same sled
/// path is rejected by sled's file lock (one writer process). The catalog fence
/// only guards commits inside that process. `open_read` never accepts a commit
/// fence.
pub struct SledWalCatalog {
    tree: sled::Tree,
    fence: Option<CatalogFence>,
    write: Mutex<()>,
}

impl SledWalCatalog {
    pub fn open(tree: sled::Tree, fence: CatalogFence) -> Self {
        Self {
            tree,
            fence: Some(fence),
            write: Mutex::new(()),
        }
    }

    pub fn open_read(tree: sled::Tree) -> Self {
        Self {
            tree,
            fence: None,
            write: Mutex::new(()),
        }
    }

    fn check_fence(&self, fence: CatalogFence) -> Result<(), CatalogError> {
        match self.fence {
            Some(owned) if owned.owner == fence.owner && owned.epoch == fence.epoch => Ok(()),
            _ => Err(CatalogError::Fenced),
        }
    }
}

impl WalCatalogRead for SledWalCatalog {
    fn get(&self, key: &CatalogKey) -> Result<Option<CatalogRow>, CatalogError> {
        let bytes = self
            .tree
            .get(key.encode().as_bytes())
            .map_err(|err| CatalogError::Io(err.to_string()))?;
        bytes.map(|v| CatalogRow::decode(&v)).transpose()
    }

    fn query_prefix(
        &self,
        prefix: CatalogPrefix,
        after: Option<&CatalogKey>,
        limit: usize,
    ) -> Result<Vec<(CatalogKey, CatalogRow)>, CatalogError> {
        let p = prefix.as_str();
        let start = after.map(CatalogKey::encode);
        let mut out = Vec::new();
        for item in self.tree.scan_prefix(p.as_bytes()) {
            let (sk, bytes) = item.map_err(|err| CatalogError::Io(err.to_string()))?;
            let sk =
                std::str::from_utf8(&sk).map_err(|err| CatalogError::Codec(err.to_string()))?;
            if let Some(start) = start.as_ref() {
                if sk <= start.as_str() {
                    continue;
                }
            }
            let key = CatalogKey::decode(sk).map_err(|err| CatalogError::Codec(err.to_string()))?;
            out.push((key, CatalogRow::decode(&bytes)?));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }
}

impl WalCatalogStore for SledWalCatalog {
    fn commit(&self, txn: &CatalogTxn, fence: CatalogFence) -> Result<(), CatalogError> {
        let _guard = self.write.lock().expect("sled catalog poisoned");
        self.check_fence(fence)?;
        let planned = eval_ops(txn, |key| {
            self.tree
                .get(key.encode().as_bytes())
                .map(|v| v.map(|bytes| bytes.to_vec()))
                .map_err(|err| CatalogError::Io(err.to_string()))
        })?;
        self.tree
            .transaction(|tx| {
                for applied in &planned {
                    match applied {
                        Applied::Put { key, bytes } => {
                            tx.insert(key.encode().as_bytes(), bytes.as_slice())?;
                        }
                        Applied::Delete { key } => {
                            tx.remove(key.encode().as_bytes())?;
                        }
                    }
                }
                Ok(())
            })
            .map_err(|err| match err {
                sled::transaction::TransactionError::Abort(()) => CatalogError::ConditionFailed,
                sled::transaction::TransactionError::Storage(err) => {
                    CatalogError::Io(err.to_string())
                }
            })?;
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
        self.tree
            .flush()
            .map(|_| ())
            .map_err(|err| CatalogError::Io(err.to_string()))
    }
}
