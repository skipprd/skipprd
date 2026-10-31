use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode, Readable};

use crate::error::CatalogError;
use crate::key::{CatalogKey, CatalogPrefix};
use crate::kv::{KvOp, SkipprStore, SkipprStoreRead};
use crate::memory_kv::{catalog_commit, catalog_delete_prefix, catalog_get, catalog_query_prefix};
use crate::row::{CatalogFence, CatalogRow};
use crate::store::{WalCatalogRead, WalCatalogStore};
use crate::txn::CatalogTxn;

struct FjallInner {
    db: Database,
    ks: Keyspace,
    fence: CatalogFence,
    _lock: File,
    dir: PathBuf,
}

#[derive(Clone)]
pub struct FjallSkipprStore {
    inner: Arc<FjallInner>,
}

impl FjallSkipprStore {
    pub fn open(dir: impl AsRef<Path>, fence: CatalogFence) -> Result<Self, CatalogError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|err| CatalogError::Io(err.to_string()))?;
        let lock_path = dir.join("writer.lock");
        let lock = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|err| CatalogError::Io(err.to_string()))?;
        lock.try_lock().map_err(|_| {
            CatalogError::Io(format!(
                "Failed opening Skippr Store at {}. Is another instance already writing this DATA_DIR?",
                dir.display()
            ))
        })?;
        let db = Database::builder(dir.join("fjall"))
            .open()
            .map_err(|err| CatalogError::Io(err.to_string()))?;
        let ks = db
            .keyspace("kv", KeyspaceCreateOptions::default)
            .map_err(|err| CatalogError::Io(err.to_string()))?;
        Ok(Self {
            inner: Arc::new(FjallInner {
                db,
                ks,
                fence,
                _lock: lock,
                dir,
            }),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    fn check_fence(&self, fence: CatalogFence) -> Result<(), CatalogError> {
        let owned = self.inner.fence;
        if owned.owner == fence.owner && owned.epoch == fence.epoch {
            Ok(())
        } else {
            Err(CatalogError::Fenced)
        }
    }
}

impl SkipprStoreRead for FjallSkipprStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, CatalogError> {
        match self
            .inner
            .ks
            .get(key)
            .map_err(|err| CatalogError::Io(err.to_string()))?
        {
            Some(value) => Ok(Some(value.to_vec())),
            None => Ok(None),
        }
    }

    fn query_prefix(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CatalogError> {
        query_prefix_iter(self.inner.ks.prefix(prefix), after, limit)
    }
}

impl SkipprStore for FjallSkipprStore {
    fn commit(&self, ops: &[KvOp], fence: CatalogFence) -> Result<(), CatalogError> {
        self.check_fence(fence)?;
        let mut batch = self.inner.db.batch();
        for op in ops {
            match op {
                KvOp::Put { key, value } => batch.insert(&self.inner.ks, key, value),
                KvOp::Delete { key } => batch.remove(&self.inner.ks, key),
            }
        }
        batch
            .commit()
            .map_err(|err| CatalogError::Io(err.to_string()))?;
        Ok(())
    }

    fn flush_durable(&self) -> Result<(), CatalogError> {
        self.inner
            .db
            .persist(PersistMode::SyncAll)
            .map_err(|err| CatalogError::Io(err.to_string()))
    }

    fn pin(&self) -> Result<Arc<dyn SkipprStoreRead>, CatalogError> {
        Ok(Arc::new(FjallPin {
            ks: self.inner.ks.clone(),
            snapshot: self.inner.db.snapshot(),
        }))
    }
}

struct FjallPin {
    ks: Keyspace,
    snapshot: fjall::Snapshot,
}

impl SkipprStoreRead for FjallPin {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, CatalogError> {
        match self
            .snapshot
            .get(&self.ks, key)
            .map_err(|err| CatalogError::Io(err.to_string()))?
        {
            Some(value) => Ok(Some(value.to_vec())),
            None => Ok(None),
        }
    }

    fn query_prefix(
        &self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CatalogError> {
        query_prefix_iter(self.snapshot.prefix(&self.ks, prefix), after, limit)
    }
}

fn query_prefix_iter(
    iter: impl Iterator<Item = fjall::Guard>,
    after: Option<&[u8]>,
    limit: usize,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CatalogError> {
    let mut out = Vec::new();
    for guard in iter {
        let (key, value) = guard
            .into_inner()
            .map_err(|err| CatalogError::Io(err.to_string()))?;
        if let Some(after) = after {
            if key.as_ref() <= after {
                continue;
            }
        }
        out.push((key.to_vec(), value.to_vec()));
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

impl WalCatalogRead for FjallSkipprStore {
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

impl WalCatalogStore for FjallSkipprStore {
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
