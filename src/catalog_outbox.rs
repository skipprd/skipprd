use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runtime_plugins::protocol::{CatalogIntent, CatalogIntentIdentity};

pub const CATALOG_OUTBOX_FORMAT_VERSION: u32 = 1;
const MAX_ENTRY_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CatalogOutboxPersistSummary {
    pub inserted: usize,
    pub coalesced: usize,
    pub updated: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConditionalMutationResult {
    Applied,
    Stale,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingCatalogIntent {
    pub id: String,
    pub intent: CatalogIntent,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub attempts: u32,
    pub next_attempt_at_ms: u64,
    pub last_error: Option<String>,
    pub terminal: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct CatalogOutboxEnvelope {
    version: u32,
    checksum_sha256: String,
    entry: PendingCatalogIntent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IndexedIntentMetadata {
    created_at_ms: u64,
    updated_at_ms: u64,
    next_attempt_at_ms: u64,
    terminal: bool,
    generation_sha256: [u8; 32],
    intent_sha256: [u8; 32],
}

impl IndexedIntentMetadata {
    fn from_entry(entry: &PendingCatalogIntent) -> io::Result<Self> {
        Ok(Self {
            created_at_ms: entry.created_at_ms,
            updated_at_ms: entry.updated_at_ms,
            next_attempt_at_ms: entry.next_attempt_at_ms,
            terminal: entry.terminal,
            generation_sha256: entry_generation(entry)?,
            intent_sha256: intent_generation(&entry.intent)?,
        })
    }
}

#[derive(Debug, Default)]
struct OutboxIndex {
    entries: BTreeMap<String, IndexedIntentMetadata>,
    due: BTreeSet<(u64, String)>,
    created: BTreeSet<(u64, String)>,
    terminal_count: usize,
    oldest_created_at_ms: Option<u64>,
    directory_sync_required: bool,
}

impl OutboxIndex {
    fn upsert(&mut self, id: String, metadata: IndexedIntentMetadata) {
        self.remove(&id);
        if metadata.terminal {
            self.terminal_count += 1;
        } else {
            self.due.insert((metadata.next_attempt_at_ms, id.clone()));
        }
        self.created.insert((metadata.created_at_ms, id.clone()));
        self.entries.insert(id, metadata);
        self.oldest_created_at_ms = self.created.first().map(|(created, _)| *created);
    }

    fn remove(&mut self, id: &str) -> Option<IndexedIntentMetadata> {
        let existing = self.entries.remove(id)?;
        if existing.terminal {
            self.terminal_count = self.terminal_count.saturating_sub(1);
        } else {
            self.due
                .remove(&(existing.next_attempt_at_ms, id.to_string()));
        }
        self.created
            .remove(&(existing.created_at_ms, id.to_string()));
        self.oldest_created_at_ms = self.created.first().map(|(created, _)| *created);
        Some(existing)
    }

    fn due_ids(&self, now_ms: u64, limit: usize) -> Vec<String> {
        self.due
            .range(..=(now_ms, String::from(char::MAX)))
            .take(limit)
            .map(|(_, id)| id.clone())
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CatalogOutboxMetadataSnapshot {
    pub pending_count: usize,
    pub terminal_count: usize,
    pub oldest_created_at_ms: Option<u64>,
}

#[derive(Debug)]
pub struct CatalogOutbox {
    pending_dir: PathBuf,
    mutation_lock: std::sync::Mutex<OutboxIndex>,
    #[cfg(test)]
    entry_reads: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    persist_delay_ms: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    directory_syncs: std::sync::atomic::AtomicUsize,
}

impl CatalogOutbox {
    pub fn open(pipeline_data_dir: impl AsRef<Path>) -> io::Result<Self> {
        let pending_dir = pipeline_data_dir
            .as_ref()
            .join("segment_buffer")
            .join("catalog_outbox")
            .join("v1")
            .join("pending");
        fs::create_dir_all(&pending_dir)?;
        sync_directory(
            pending_dir
                .parent()
                .expect("catalog outbox pending directory has a parent"),
        )?;
        let index = recover_index(&pending_dir)?;
        let outbox = Self {
            pending_dir,
            mutation_lock: std::sync::Mutex::new(index),
            #[cfg(test)]
            entry_reads: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            persist_delay_ms: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            directory_syncs: std::sync::atomic::AtomicUsize::new(0),
        };
        Ok(outbox)
    }

    pub fn pending_dir(&self) -> &Path {
        &self.pending_dir
    }

    pub fn persist(&self, intents: &[CatalogIntent]) -> io::Result<CatalogOutboxPersistSummary> {
        let mut index = self
            .mutation_lock
            .lock()
            .expect("catalog outbox mutation lock poisoned");
        #[cfg(test)]
        std::thread::sleep(Duration::from_millis(
            self.persist_delay_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        ));
        let mut summary = CatalogOutboxPersistSummary::default();
        for intent in intents {
            let id = stable_intent_id(&intent.identity)?;
            let path = self.entry_path(&id);
            let now = now_ms();
            let intent_sha256 = intent_generation(intent)?;
            let entry = if let Some(existing) = index.entries.get(&id) {
                if existing.intent_sha256 == intent_sha256 {
                    summary.coalesced += 1;
                    continue;
                }
                summary.updated += 1;
                PendingCatalogIntent {
                    id: id.clone(),
                    intent: intent.clone(),
                    created_at_ms: existing.created_at_ms,
                    updated_at_ms: now,
                    attempts: 0,
                    next_attempt_at_ms: 0,
                    last_error: None,
                    terminal: false,
                }
            } else {
                summary.inserted += 1;
                PendingCatalogIntent {
                    id: id.clone(),
                    intent: intent.clone(),
                    created_at_ms: now,
                    updated_at_ms: now,
                    attempts: 0,
                    next_attempt_at_ms: 0,
                    last_error: None,
                    terminal: false,
                }
            };
            write_entry_atomic_rename(&path, &entry)?;
            index.directory_sync_required = true;
            index.upsert(id, IndexedIntentMetadata::from_entry(&entry)?);
        }
        if index.directory_sync_required {
            self.sync_pending_directory()?;
            index.directory_sync_required = false;
        }
        Ok(summary)
    }

    pub fn scan_pending(&self, limit: usize) -> io::Result<Vec<PendingCatalogIntent>> {
        let index = self
            .mutation_lock
            .lock()
            .expect("catalog outbox mutation lock poisoned");
        let mut pending = Vec::with_capacity(limit.min(1_024));
        for id in index.entries.keys().take(limit) {
            pending.push(self.read_indexed_entry(id)?);
        }
        Ok(pending)
    }

    pub fn scan_eligible_pending(
        &self,
        now_ms: u64,
        limit: usize,
    ) -> io::Result<Vec<PendingCatalogIntent>> {
        let index = self
            .mutation_lock
            .lock()
            .expect("catalog outbox mutation lock poisoned");
        let ids = index.due_ids(now_ms, limit);
        let mut pending = Vec::with_capacity(ids.len());
        for id in ids {
            pending.push(self.read_indexed_entry(&id)?);
        }
        Ok(pending)
    }

    pub fn metadata_snapshot(&self) -> CatalogOutboxMetadataSnapshot {
        let index = self
            .mutation_lock
            .lock()
            .expect("catalog outbox mutation lock poisoned");
        CatalogOutboxMetadataSnapshot {
            pending_count: index.entries.len(),
            terminal_count: index.terminal_count,
            oldest_created_at_ms: index.oldest_created_at_ms,
        }
    }

    pub fn mark_delivered_if(
        &self,
        expected: &PendingCatalogIntent,
    ) -> io::Result<ConditionalMutationResult> {
        let mut index = self
            .mutation_lock
            .lock()
            .expect("catalog outbox mutation lock poisoned");
        let expected_generation = entry_generation(expected)?;
        if index
            .entries
            .get(&expected.id)
            .is_none_or(|metadata| metadata.generation_sha256 != expected_generation)
        {
            return Ok(ConditionalMutationResult::Stale);
        }
        let path = self.entry_path(&expected.id);
        let current = match self.read_indexed_entry(&expected.id) {
            Ok(current) => current,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(ConditionalMutationResult::Stale)
            }
            Err(err) => return Err(err),
        };
        if current != *expected {
            return Ok(ConditionalMutationResult::Stale);
        }
        fs::remove_file(&path)?;
        index.remove(&expected.id);
        index.directory_sync_required = true;
        self.sync_pending_directory()?;
        index.directory_sync_required = false;
        Ok(ConditionalMutationResult::Applied)
    }

    pub fn record_failure_if(
        &self,
        expected: &PendingCatalogIntent,
        error: impl Into<String>,
        retry_after: Option<Duration>,
        terminal: bool,
    ) -> io::Result<ConditionalMutationResult> {
        let mut index = self
            .mutation_lock
            .lock()
            .expect("catalog outbox mutation lock poisoned");
        let expected_generation = entry_generation(expected)?;
        if index
            .entries
            .get(&expected.id)
            .is_none_or(|metadata| metadata.generation_sha256 != expected_generation)
        {
            return Ok(ConditionalMutationResult::Stale);
        }
        let path = self.entry_path(&expected.id);
        let mut entry = match self.read_indexed_entry(&expected.id) {
            Ok(entry) => entry,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(ConditionalMutationResult::Stale)
            }
            Err(err) => return Err(err),
        };
        if entry != *expected {
            return Ok(ConditionalMutationResult::Stale);
        }
        entry.attempts = entry.attempts.saturating_add(1);
        entry.updated_at_ms = now_ms();
        entry.next_attempt_at_ms = retry_after
            .map(|delay| entry.updated_at_ms.saturating_add(delay.as_millis() as u64))
            .unwrap_or(0);
        entry.last_error = Some(error.into());
        entry.terminal = terminal;
        write_entry_atomic_rename(&path, &entry)?;
        index.upsert(
            expected.id.clone(),
            IndexedIntentMetadata::from_entry(&entry)?,
        );
        index.directory_sync_required = true;
        self.sync_pending_directory()?;
        index.directory_sync_required = false;
        Ok(ConditionalMutationResult::Applied)
    }

    fn entry_path(&self, id: &str) -> PathBuf {
        self.pending_dir.join(format!("{id}.json"))
    }

    fn read_indexed_entry(&self, id: &str) -> io::Result<PendingCatalogIntent> {
        #[cfg(test)]
        self.entry_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        read_entry(&self.entry_path(id))
    }

    fn sync_pending_directory(&self) -> io::Result<()> {
        sync_directory(&self.pending_dir)?;
        #[cfg(test)]
        self.directory_syncs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    #[cfg(test)]
    fn reset_entry_read_count(&self) {
        self.entry_reads
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(test)]
    fn entry_read_count(&self) -> usize {
        self.entry_reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn set_persist_delay(&self, delay: Duration) {
        self.persist_delay_ms.store(
            delay.as_millis() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    #[cfg(test)]
    fn directory_sync_count(&self) -> usize {
        self.directory_syncs
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn recover_index(pending_dir: &Path) -> io::Result<OutboxIndex> {
    let mut index = OutboxIndex::default();
    for entry in fs::read_dir(pending_dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let entry = read_entry(&path)?;
        if index.entries.contains_key(&entry.id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("duplicate catalog outbox identity '{}'", entry.id),
            ));
        }
        index.upsert(entry.id.clone(), IndexedIntentMetadata::from_entry(&entry)?);
    }
    Ok(index)
}

pub fn stable_intent_id(identity: &CatalogIntentIdentity) -> io::Result<String> {
    let bytes = serde_json::to_vec(identity)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn entry_generation(entry: &PendingCatalogIntent) -> io::Result<[u8; 32]> {
    let bytes =
        serde_json::to_vec(entry).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(Sha256::digest(bytes).into())
}

fn intent_generation(intent: &CatalogIntent) -> io::Result<[u8; 32]> {
    let bytes = serde_json::to_vec(intent)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(Sha256::digest(bytes).into())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn checksum(entry: &PendingCatalogIntent) -> io::Result<String> {
    let bytes =
        serde_json::to_vec(entry).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn read_entry(path: &Path) -> io::Result<PendingCatalogIntent> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_ENTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "catalog outbox entry '{}' exceeds {} bytes",
                path.display(),
                MAX_ENTRY_BYTES
            ),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(MAX_ENTRY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let envelope: CatalogOutboxEnvelope = serde_json::from_slice(&bytes).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("corrupt catalog outbox entry '{}': {err}", path.display()),
        )
    })?;
    if envelope.version != CATALOG_OUTBOX_FORMAT_VERSION
        || checksum(&envelope.entry)? != envelope.checksum_sha256
        || stable_intent_id(&envelope.entry.intent.identity)? != envelope.entry.id
        || path.file_stem().and_then(|stem| stem.to_str()) != Some(envelope.entry.id.as_str())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "catalog outbox entry '{}' failed version, checksum, or identity validation",
                path.display()
            ),
        ));
    }
    Ok(envelope.entry)
}

fn write_entry_atomic_rename(path: &Path, entry: &PendingCatalogIntent) -> io::Result<()> {
    let envelope = CatalogOutboxEnvelope {
        version: CATALOG_OUTBOX_FORMAT_VERSION,
        checksum_sha256: checksum(entry)?,
        entry: entry.clone(),
    };
    let bytes = serde_json::to_vec(&envelope)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if bytes.len() as u64 > MAX_ENTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "catalog outbox entry exceeds durable format limit",
        ));
    }
    let tmp = path.with_extension(format!("json.tmp-{}-{}", std::process::id(), now_ms()));
    let mut file = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
    if let Err(err) = (|| -> io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })() {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_plugins::protocol::{CatalogIntentKind, CATALOG_INTENT_VERSION};

    fn intent_for(key: &str, location: &str) -> CatalogIntent {
        CatalogIntent {
            version: CATALOG_INTENT_VERSION,
            identity: CatalogIntentIdentity {
                sink_ref: "primary".into(),
                namespace: "events".into(),
                kind: CatalogIntentKind::UpsertPartition,
                key: key.into(),
            },
            payload_json: serde_json::json!({ "location": location }).to_string(),
        }
    }

    fn intent(location: &str) -> CatalogIntent {
        intent_for("day=2026-07-30", location)
    }

    #[test]
    fn duplicate_identity_coalesces_and_location_change_updates() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = CatalogOutbox::open(temp.path()).unwrap();
        assert_eq!(outbox.persist(&[intent("s3://a")]).unwrap().inserted, 1);
        assert_eq!(outbox.persist(&[intent("s3://a")]).unwrap().coalesced, 1);
        assert_eq!(outbox.persist(&[intent("s3://b")]).unwrap().updated, 1);
        let pending = outbox.scan_pending(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].intent.payload_json.contains("s3://b"));
    }

    #[test]
    fn retry_metadata_survives_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = CatalogOutbox::open(temp.path()).unwrap();
        outbox.persist(&[intent("s3://a")]).unwrap();
        let pending = outbox.scan_pending(1).unwrap()[0].clone();
        outbox
            .record_failure_if(&pending, "throttled", Some(Duration::from_secs(1)), false)
            .unwrap();
        let recovered = CatalogOutbox::open(temp.path())
            .unwrap()
            .scan_pending(1)
            .unwrap();
        assert_eq!(recovered[0].attempts, 1);
        assert_eq!(recovered[0].last_error.as_deref(), Some("throttled"));
        assert!(recovered[0].next_attempt_at_ms > 0);
    }

    #[test]
    fn corrupt_entry_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = CatalogOutbox::open(temp.path()).unwrap();
        fs::write(outbox.pending_dir().join("corrupt.json"), b"not-json").unwrap();
        let err = CatalogOutbox::open(temp.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn stale_delivery_does_not_delete_changed_location() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = CatalogOutbox::open(temp.path()).unwrap();
        outbox.persist(&[intent("s3://a")]).unwrap();
        let scanned = outbox.scan_pending(1).unwrap().remove(0);
        outbox.persist(&[intent("s3://b")]).unwrap();

        assert_eq!(
            outbox.mark_delivered_if(&scanned).unwrap(),
            ConditionalMutationResult::Stale
        );
        let current = outbox.scan_pending(1).unwrap().remove(0);
        assert!(current.intent.payload_json.contains("s3://b"));
    }

    #[test]
    fn stale_failure_does_not_mutate_changed_location() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = CatalogOutbox::open(temp.path()).unwrap();
        outbox.persist(&[intent("s3://a")]).unwrap();
        let scanned = outbox.scan_pending(1).unwrap().remove(0);
        outbox.persist(&[intent("s3://b")]).unwrap();

        assert_eq!(
            outbox
                .record_failure_if(&scanned, "stale failure", None, true)
                .unwrap(),
            ConditionalMutationResult::Stale
        );
        let current = outbox.scan_pending(1).unwrap().remove(0);
        assert!(current.intent.payload_json.contains("s3://b"));
        assert_eq!(current.attempts, 0);
        assert_eq!(current.last_error, None);
        assert!(!current.terminal);
    }

    #[test]
    fn eligible_scan_skips_terminal_and_future_entries_without_spending_limit() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = CatalogOutbox::open(temp.path()).unwrap();
        let intents = (0..5)
            .map(|index| intent_for(&format!("day={index}"), &format!("s3://{index}")))
            .collect::<Vec<_>>();
        outbox.persist(&intents).unwrap();
        let mut entries = outbox.scan_pending(10).unwrap();
        entries.sort_by(|left, right| left.intent.identity.key.cmp(&right.intent.identity.key));
        for entry in entries.iter().take(2) {
            outbox
                .record_failure_if(entry, "terminal", None, true)
                .unwrap();
        }
        outbox
            .record_failure_if(
                &entries[2],
                "future",
                Some(Duration::from_secs(3_600)),
                false,
            )
            .unwrap();

        let eligible = outbox.scan_eligible_pending(now_ms(), 2).unwrap();
        assert_eq!(eligible.len(), 2);
        assert!(eligible
            .iter()
            .all(|entry| { matches!(entry.intent.identity.key.as_str(), "day=3" | "day=4") }));
        let snapshot = outbox.metadata_snapshot();
        assert_eq!(snapshot.pending_count, 5);
        assert_eq!(snapshot.terminal_count, 2);
        assert!(snapshot.oldest_created_at_ms.is_some());
    }

    #[test]
    fn copied_envelope_under_wrong_filename_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = CatalogOutbox::open(temp.path()).unwrap();
        outbox.persist(&[intent("s3://a")]).unwrap();
        let original = fs::read_dir(outbox.pending_dir())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::copy(original, outbox.pending_dir().join("wrong-id.json")).unwrap();

        let err = CatalogOutbox::open(temp.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("identity validation"));
    }

    #[test]
    fn indexed_hot_paths_read_only_new_or_selected_entries() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = CatalogOutbox::open(temp.path()).unwrap();
        let backlog = (0..128)
            .map(|index| intent_for(&format!("day={index:03}"), &format!("s3://{index:03}")))
            .collect::<Vec<_>>();
        outbox.persist(&backlog).unwrap();
        assert_eq!(outbox.metadata_snapshot().pending_count, 128);
        assert_eq!(
            outbox.directory_sync_count(),
            1,
            "one multi-intent persist must coalesce directory fsync after all renames"
        );

        outbox.reset_entry_read_count();
        outbox
            .persist(&[intent_for("day=new", "s3://new")])
            .unwrap();
        crate::metrics::counters::refresh_catalog_outbox_metrics(&outbox).unwrap();
        assert_eq!(
            outbox.entry_read_count(),
            0,
            "persist and metric publication must not reread backlog payloads"
        );

        outbox.reset_entry_read_count();
        let selected = outbox.scan_eligible_pending(now_ms(), 7).unwrap();
        assert_eq!(selected.len(), 7);
        assert_eq!(
            outbox.entry_read_count(),
            7,
            "due selection must read only selected payloads"
        );

        outbox.reset_entry_read_count();
        let deterministic = outbox.scan_pending(3).unwrap();
        assert_eq!(deterministic.len(), 3);
        assert!(deterministic
            .windows(2)
            .all(|window| window[0].id < window[1].id));
        assert_eq!(outbox.entry_read_count(), 3);
    }

    #[test]
    fn recovery_rebuilds_exact_metadata_and_due_indexes() {
        let temp = tempfile::tempdir().unwrap();
        let outbox = CatalogOutbox::open(temp.path()).unwrap();
        outbox
            .persist(&[
                intent_for("day=due", "s3://due"),
                intent_for("day=future", "s3://future"),
                intent_for("day=terminal", "s3://terminal"),
            ])
            .unwrap();
        let mut entries = outbox.scan_pending(10).unwrap();
        entries.sort_by(|left, right| left.intent.identity.key.cmp(&right.intent.identity.key));
        let future = entries
            .iter()
            .find(|entry| entry.intent.identity.key == "day=future")
            .unwrap();
        outbox
            .record_failure_if(future, "future", Some(Duration::from_secs(3_600)), false)
            .unwrap();
        let terminal = entries
            .iter()
            .find(|entry| entry.intent.identity.key == "day=terminal")
            .unwrap();
        outbox
            .record_failure_if(terminal, "terminal", None, true)
            .unwrap();
        let before = outbox.metadata_snapshot();
        drop(outbox);

        let recovered = CatalogOutbox::open(temp.path()).unwrap();
        assert_eq!(recovered.metadata_snapshot(), before);
        recovered.reset_entry_read_count();
        let due = recovered.scan_eligible_pending(now_ms(), 10).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].intent.identity.key, "day=due");
        assert_eq!(recovered.entry_read_count(), 1);
    }
}
