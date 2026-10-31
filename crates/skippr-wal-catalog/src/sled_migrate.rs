//! Read-only copy of leftover `{DATA_DIR}/db` sled trees into fjall.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::CatalogError;
use crate::kv::{KvOp, SkipprStore, SkipprStoreRead, StoreFamily, STORE_FORMAT_VERSION};
use crate::row::CatalogFence;

const CURSOR_KEY: &[u8] = b"sled_migrate/cursor";
const FORMAT_KEY: &[u8] = b"format_version";
const COPY_BATCH: usize = 64;

pub const SLED_TREES: [&str; 3] = ["offsets", "wal_catalog", "stream_state"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MigrateStatus {
    NoSled,
    InProgress { tree: String, remaining_hint: usize },
    Complete { archived_to: PathBuf },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Eq, PartialEq)]
struct Cursor {
    tree_index: usize,
    last_key: Vec<u8>,
    keys_copied: u64,
    checksum: [u8; 32],
}

impl Cursor {
    fn signed(tree_index: usize, last_key: Vec<u8>, keys_copied: u64) -> Self {
        let checksum = cursor_checksum(tree_index, &last_key, keys_copied);
        Self {
            tree_index,
            last_key,
            keys_copied,
            checksum,
        }
    }

    fn verify(&self) -> Result<(), CatalogError> {
        let expect = cursor_checksum(self.tree_index, &self.last_key, self.keys_copied);
        if expect != self.checksum {
            return Err(CatalogError::Io(
                "sled migrate cursor checksum mismatch".into(),
            ));
        }
        Ok(())
    }
}

fn cursor_checksum(tree_index: usize, last_key: &[u8], keys_copied: u64) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(tree_index.to_le_bytes());
    h.update(keys_copied.to_le_bytes());
    h.update(last_key);
    h.finalize().into()
}

fn family_for_sled_key(tree: &str, key: &[u8]) -> StoreFamily {
    match tree {
        "offsets" if key.starts_with(b"migration#") => StoreFamily::Journal,
        "offsets" => StoreFamily::Offsets,
        "wal_catalog" => StoreFamily::Catalog,
        "stream_state" => StoreFamily::StreamState,
        _ => StoreFamily::StoreMeta,
    }
}

pub fn migrate_sled_db(
    store: &dyn SkipprStore,
    fence: CatalogFence,
    sled_path: &Path,
    seq: u64,
) -> Result<MigrateStatus, CatalogError> {
    ensure_format(store, fence)?;
    if !sled_path.exists() {
        return Ok(MigrateStatus::NoSled);
    }
    if sled_path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("db.sled-migrated-"))
    {
        return Ok(MigrateStatus::NoSled);
    }

    // sled 0.34 has no read-only open. Contract: never insert, commit, or create trees.
    let db = sled::open(sled_path).map_err(|err| {
        CatalogError::Io(format!("unreadable sled at {}: {err}", sled_path.display()))
    })?;

    let mut cursor = load_cursor(store)?;
    loop {
        if cursor.tree_index >= SLED_TREES.len() {
            drop(db);
            store.flush_durable()?;
            let archived = archive_sled(sled_path, seq)?;
            return Ok(MigrateStatus::Complete {
                archived_to: archived,
            });
        }
        let tree_name = SLED_TREES[cursor.tree_index];
        let Some(tree) = open_existing_tree(&db, tree_name)? else {
            cursor = Cursor::signed(cursor.tree_index + 1, Vec::new(), 0);
            persist_cursor(store, fence, &cursor)?;
            continue;
        };
        let mut ops = Vec::new();
        let mut copied = 0usize;
        let mut last = cursor.last_key.clone();
        for item in tree.iter() {
            let (key, value) = item.map_err(|err| CatalogError::Io(err.to_string()))?;
            if !cursor.last_key.is_empty() && key.as_ref() <= cursor.last_key.as_slice() {
                continue;
            }
            let family = family_for_sled_key(tree_name, &key);
            ops.push(KvOp::Put {
                key: family.key(&key),
                value: value.to_vec(),
            });
            last = key.to_vec();
            copied += 1;
            if copied >= COPY_BATCH {
                break;
            }
        }
        if ops.is_empty() {
            cursor = Cursor::signed(cursor.tree_index + 1, Vec::new(), 0);
            persist_cursor(store, fence, &cursor)?;
            continue;
        }
        store.commit(&ops, fence)?;
        cursor = Cursor::signed(cursor.tree_index, last, cursor.keys_copied + copied as u64);
        persist_cursor(store, fence, &cursor)?;
        return Ok(MigrateStatus::InProgress {
            tree: tree_name.to_string(),
            remaining_hint: COPY_BATCH,
        });
    }
}

fn ensure_format(store: &dyn SkipprStore, fence: CatalogFence) -> Result<(), CatalogError> {
    let key = StoreFamily::StoreMeta.key(FORMAT_KEY);
    if store.get(&key)?.is_some() {
        return Ok(());
    }
    store.commit(
        &[KvOp::Put {
            key,
            value: STORE_FORMAT_VERSION.to_le_bytes().to_vec(),
        }],
        fence,
    )
}

fn load_cursor(store: &dyn SkipprStoreRead) -> Result<Cursor, CatalogError> {
    match store.get(&StoreFamily::StoreMeta.key(CURSOR_KEY))? {
        None => Ok(Cursor::signed(0, Vec::new(), 0)),
        Some(bytes) => {
            let cursor: Cursor =
                bincode::deserialize(&bytes).map_err(|err| CatalogError::Codec(err.to_string()))?;
            cursor.verify()?;
            Ok(cursor)
        }
    }
}

fn persist_cursor(
    store: &dyn SkipprStore,
    fence: CatalogFence,
    cursor: &Cursor,
) -> Result<(), CatalogError> {
    let bytes = bincode::serialize(cursor).map_err(|err| CatalogError::Codec(err.to_string()))?;
    store.commit(
        &[KvOp::Put {
            key: StoreFamily::StoreMeta.key(CURSOR_KEY),
            value: bytes,
        }],
        fence,
    )
}

fn open_existing_tree(db: &sled::Db, name: &str) -> Result<Option<sled::Tree>, CatalogError> {
    let want = name.as_bytes();
    if !db.tree_names().iter().any(|n| n.as_ref() == want) {
        return Ok(None);
    }
    db.open_tree(name)
        .map(Some)
        .map_err(|err| CatalogError::Io(err.to_string()))
}

fn archive_sled(sled_path: &Path, seq: u64) -> Result<PathBuf, CatalogError> {
    let parent = sled_path.parent().unwrap_or(sled_path);
    let archived = parent.join(format!("db.sled-migrated-{seq}"));
    std::fs::rename(sled_path, &archived).map_err(|err| CatalogError::Io(err.to_string()))?;
    Ok(archived)
}

pub fn lookup_miss_in_sled(
    sled_path: &Path,
    family: StoreFamily,
    rest: &[u8],
) -> Result<Option<Vec<u8>>, CatalogError> {
    if !sled_path.exists() {
        return Ok(None);
    }
    let tree_name = match family {
        StoreFamily::Offsets | StoreFamily::Journal => "offsets",
        StoreFamily::Catalog => "wal_catalog",
        StoreFamily::StreamState => "stream_state",
        _ => return Ok(None),
    };
    // sled 0.34 has no read-only open. Contract: never insert, commit, or create trees.
    let db = sled::open(sled_path).map_err(|err| CatalogError::Io(err.to_string()))?;
    let Some(tree) = open_existing_tree(&db, tree_name)? else {
        return Ok(None);
    };
    tree.get(rest)
        .map(|v| v.map(|bytes| bytes.to_vec()))
        .map_err(|err| CatalogError::Io(err.to_string()))
}
