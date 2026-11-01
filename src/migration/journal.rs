use serde::{Deserialize, Serialize};
use skippr_wal_catalog::{CatalogFence, KvOp, SkipprStore, SkipprStoreRead, StoreFamily};
use std::fmt::{Display, Formatter};

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

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MigrationId(String);

impl MigrationId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn query(checksum: MigrationChecksum) -> Self {
        Self(format!("query-{}", checksum.0))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MigrationChecksum(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MigrationDirection {
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MigrationPhase {
    WalRewritten,
    DestApplied,
    Committed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRecord {
    pub id: MigrationId,
    pub checksum: MigrationChecksum,
    pub direction: MigrationDirection,
    pub phase: MigrationPhase,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MigrationLog {
    pub records: Vec<MigrationRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationApply {
    pub id: MigrationId,
    pub checksum: MigrationChecksum,
    pub direction: MigrationDirection,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JournalError {
    #[error("migration '{id}' checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch {
        id: MigrationId,
        expected: MigrationChecksum,
        actual: MigrationChecksum,
    },
    #[error("migration journal: {0}")]
    Store(String),
}

impl Display for MigrationChecksum {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Display for MigrationId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub fn journal_key(workspace: &str, pipeline: &str) -> Result<JournalKey, JournalError> {
    JournalKey::migration(workspace, pipeline).map_err(JournalError::Store)
}

fn journal_store_key(key: &JournalKey) -> Vec<u8> {
    StoreFamily::Journal.key(key.as_str().as_bytes())
}

pub fn latest_phase(
    log: &MigrationLog,
    apply: &MigrationApply,
) -> Result<Option<MigrationPhase>, JournalError> {
    match log
        .records
        .iter()
        .rev()
        .find(|record| record.id == apply.id)
    {
        Some(record) if record.checksum != apply.checksum => Err(JournalError::ChecksumMismatch {
            id: apply.id.clone(),
            expected: record.checksum.clone(),
            actual: apply.checksum.clone(),
        }),
        Some(record) if record.direction != apply.direction => Ok(None),
        Some(record) => Ok(Some(record.phase)),
        None => Ok(None),
    }
}

pub fn committed(log: &MigrationLog, apply: &MigrationApply) -> Result<bool, JournalError> {
    Ok(latest_phase(log, apply)? == Some(MigrationPhase::Committed))
}

pub fn load(store: &dyn SkipprStoreRead, key: &JournalKey) -> Result<MigrationLog, JournalError> {
    match store
        .get(&journal_store_key(key))
        .map_err(|err| JournalError::Store(err.to_string()))?
    {
        None => Ok(MigrationLog::default()),
        Some(bytes) => {
            bincode::deserialize(&bytes).map_err(|err| JournalError::Store(err.to_string()))
        }
    }
}

pub fn commit(
    store: &dyn SkipprStore,
    fence: CatalogFence,
    key: &JournalKey,
    apply: &MigrationApply,
    phase: MigrationPhase,
) -> Result<(), JournalError> {
    let mut log = load(store, key)?;
    if let Some(existing) = log
        .records
        .iter()
        .rev()
        .find(|record| record.id == apply.id)
    {
        if existing.checksum != apply.checksum {
            return Err(JournalError::ChecksumMismatch {
                id: apply.id.clone(),
                expected: existing.checksum.clone(),
                actual: apply.checksum.clone(),
            });
        }
    }
    log.records.push(MigrationRecord {
        id: apply.id.clone(),
        checksum: apply.checksum.clone(),
        direction: apply.direction,
        phase,
    });
    let bytes = bincode::serialize(&log).map_err(|err| JournalError::Store(err.to_string()))?;
    store
        .commit(
            &[KvOp::Put {
                key: journal_store_key(key),
                value: bytes,
            }],
            fence,
        )
        .map_err(|err| JournalError::Store(err.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_wal_catalog::MemorySkipprStore;

    fn mem() -> (MemorySkipprStore, CatalogFence) {
        let fence = CatalogFence::local();
        (MemorySkipprStore::open(fence), fence)
    }

    #[test]
    fn journal_uses_skippr_store_not_offset_store() {
        let src = include_str!("journal.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        assert!(prod.contains("SkipprStore"));
        assert!(prod.contains("SkipprStoreRead"));
        assert!(prod.contains("StoreFamily::Journal"));
        assert!(!prod.contains("OffsetStore"));
        assert!(!prod.contains("get_journal"));
        assert!(!prod.contains("fetch_and_update_journal"));
    }

    #[test]
    fn checksum_mismatch_is_fail_closed() {
        let (store, fence) = mem();
        let key = journal_key("ws", "p").unwrap();
        let first = MigrationApply {
            id: MigrationId::new("m1"),
            checksum: MigrationChecksum::of("one"),
            direction: MigrationDirection::Up,
        };
        commit(&store, fence, &key, &first, MigrationPhase::Committed).unwrap();
        let second = MigrationApply {
            id: MigrationId::new("m1"),
            checksum: MigrationChecksum::of("two"),
            direction: MigrationDirection::Up,
        };
        let err = commit(&store, fence, &key, &second, MigrationPhase::Committed).unwrap_err();
        assert!(matches!(err, JournalError::ChecksumMismatch { .. }));
    }

    #[test]
    fn latest_phase_is_applied_truth_for_resume() {
        let (store, fence) = mem();
        let key = journal_key("ws", "p").unwrap();
        let apply = MigrationApply {
            id: MigrationId::new("m1"),
            checksum: MigrationChecksum::of("one"),
            direction: MigrationDirection::Up,
        };
        assert_eq!(
            latest_phase(&load(&store, &key).unwrap(), &apply).unwrap(),
            None
        );
        commit(&store, fence, &key, &apply, MigrationPhase::WalRewritten).unwrap();
        assert_eq!(
            latest_phase(&load(&store, &key).unwrap(), &apply).unwrap(),
            Some(MigrationPhase::WalRewritten)
        );
        commit(&store, fence, &key, &apply, MigrationPhase::DestApplied).unwrap();
        assert_eq!(
            latest_phase(&load(&store, &key).unwrap(), &apply).unwrap(),
            Some(MigrationPhase::DestApplied)
        );
        assert!(!committed(&load(&store, &key).unwrap(), &apply).unwrap());
    }
}
