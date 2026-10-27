use crate::error::CatalogError;
use crate::key::{CatalogKey, CatalogPrefix};
use crate::row::{CatalogFence, CatalogRow};
use crate::txn::CatalogTxn;

pub trait WalCatalogRead: Send + Sync {
    fn get(&self, key: &CatalogKey) -> Result<Option<CatalogRow>, CatalogError>;
    fn query_prefix(
        &self,
        prefix: CatalogPrefix,
        after: Option<&CatalogKey>,
        limit: usize,
    ) -> Result<Vec<(CatalogKey, CatalogRow)>, CatalogError>;
}

pub trait WalCatalogStore: WalCatalogRead {
    fn commit(&self, txn: &CatalogTxn, fence: CatalogFence) -> Result<(), CatalogError>;
    fn delete_prefix(&self, prefix: CatalogPrefix, fence: CatalogFence)
        -> Result<(), CatalogError>;
    fn flush_durable(&self) -> Result<(), CatalogError>;
    fn commit_durable(&self, txn: &CatalogTxn, fence: CatalogFence) -> Result<(), CatalogError> {
        self.commit(txn, fence)?;
        self.flush_durable()
    }
}

pub fn query_all(
    store: &dyn WalCatalogRead,
    prefix: CatalogPrefix,
) -> Result<Vec<(CatalogKey, CatalogRow)>, CatalogError> {
    const PAGE: usize = 100;
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = store.query_prefix(prefix, after.as_ref(), PAGE)?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|(key, _)| key.clone());
        let n = page.len();
        out.extend(page);
        if n < PAGE {
            break;
        }
    }
    Ok(out)
}

/// Discover / pipeline reset: every catalog prefix plus Seq and Format.
pub fn reset_catalog(store: &dyn WalCatalogStore, fence: CatalogFence) -> Result<(), CatalogError> {
    for prefix in CatalogPrefix::ALL {
        store.delete_prefix(prefix, fence)?;
    }
    for key in [CatalogKey::Seq, CatalogKey::Format] {
        if store.get(&key)?.is_none() {
            continue;
        }
        let mut txn = CatalogTxn::new();
        txn.delete(key, crate::row::DeleteCond::Exists)?;
        store.commit_durable(&txn, fence)?;
    }
    store.flush_durable()
}
