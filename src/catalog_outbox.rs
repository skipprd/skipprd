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

#[derive(Debug)]
pub struct CatalogOutbox {
    pending_dir: PathBuf,
    mutation_lock: std::sync::Mutex<()>,
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
        let outbox = Self {
            pending_dir,
            mutation_lock: std::sync::Mutex::new(()),
        };
        // Recovery is fail-closed: opening a pipeline with one corrupt pending
        // intent must surface the error before any entry can be dropped.
        outbox.validate_pending()?;
        Ok(outbox)
    }

    pub fn pending_dir(&self) -> &Path {
        &self.pending_dir
    }

    pub fn persist(&self, intents: &[CatalogIntent]) -> io::Result<CatalogOutboxPersistSummary> {
        let _guard = self
            .mutation_lock
            .lock()
            .expect("catalog outbox mutation lock poisoned");
        let mut summary = CatalogOutboxPersistSummary::default();
        for intent in intents {
            let id = stable_intent_id(&intent.identity)?;
            let path = self.entry_path(&id);
            let now = now_ms();
            let entry = if path.exists() {
                let mut existing = read_entry(&path)?;
                if existing.intent == *intent {
                    summary.coalesced += 1;
                    continue;
                }
                existing.intent = intent.clone();
                existing.updated_at_ms = now;
                existing.attempts = 0;
                existing.next_attempt_at_ms = 0;
                existing.last_error = None;
                existing.terminal = false;
                summary.updated += 1;
                existing
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
            write_entry_atomic(&path, &entry)?;
        }
        Ok(summary)
    }

    pub fn scan_pending(&self, limit: usize) -> io::Result<Vec<PendingCatalogIntent>> {
        let mut pending = Vec::with_capacity(limit.min(1_024));
        for entry in fs::read_dir(&self.pending_dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            pending.push(read_entry(&path)?);
            if pending.len() >= limit {
                break;
            }
        }
        Ok(pending)
    }

    pub fn scan_eligible_pending(
        &self,
        now_ms: u64,
        limit: usize,
    ) -> io::Result<Vec<PendingCatalogIntent>> {
        let mut pending = Vec::with_capacity(limit.min(1_024));
        for entry in fs::read_dir(&self.pending_dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let entry = read_entry(&path)?;
            if pending.len() < limit && !entry.terminal && entry.next_attempt_at_ms <= now_ms {
                pending.push(entry);
            }
        }
        Ok(pending)
    }

    pub fn mark_delivered_if(
        &self,
        expected: &PendingCatalogIntent,
    ) -> io::Result<ConditionalMutationResult> {
        let _guard = self
            .mutation_lock
            .lock()
            .expect("catalog outbox mutation lock poisoned");
        let path = self.entry_path(&expected.id);
        let current = match read_entry(&path) {
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
        sync_directory(&self.pending_dir)?;
        Ok(ConditionalMutationResult::Applied)
    }

    pub fn record_failure_if(
        &self,
        expected: &PendingCatalogIntent,
        error: impl Into<String>,
        retry_after: Option<Duration>,
        terminal: bool,
    ) -> io::Result<ConditionalMutationResult> {
        let _guard = self
            .mutation_lock
            .lock()
            .expect("catalog outbox mutation lock poisoned");
        let path = self.entry_path(&expected.id);
        let mut entry = match read_entry(&path) {
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
        write_entry_atomic(&path, &entry)?;
        Ok(ConditionalMutationResult::Applied)
    }

    fn entry_path(&self, id: &str) -> PathBuf {
        self.pending_dir.join(format!("{id}.json"))
    }

    fn validate_pending(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.pending_dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                read_entry(&path)?;
            }
        }
        Ok(())
    }
}

pub fn stable_intent_id(identity: &CatalogIntentIdentity) -> io::Result<String> {
    let bytes = serde_json::to_vec(identity)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok(hex::encode(Sha256::digest(bytes)))
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

fn write_entry_atomic(path: &Path, entry: &PendingCatalogIntent) -> io::Result<()> {
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
        sync_directory(
            path.parent()
                .expect("catalog outbox entry path has a parent"),
        )
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

        let err = outbox.scan_eligible_pending(now_ms(), 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("identity validation"));
    }
}
