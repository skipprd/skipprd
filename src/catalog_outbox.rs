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

#[derive(Clone, Debug)]
pub struct CatalogOutbox {
    pending_dir: PathBuf,
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
        let outbox = Self { pending_dir };
        // Recovery is fail-closed: opening a pipeline with one corrupt pending
        // intent must surface the error before any entry can be dropped.
        outbox.scan_pending(usize::MAX)?;
        Ok(outbox)
    }

    pub fn pending_dir(&self) -> &Path {
        &self.pending_dir
    }

    pub fn persist(&self, intents: &[CatalogIntent]) -> io::Result<CatalogOutboxPersistSummary> {
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
        let mut paths = fs::read_dir(&self.pending_dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()?;
        paths.retain(|path| path.extension().is_some_and(|ext| ext == "json"));
        paths.sort();
        let mut pending = Vec::with_capacity(paths.len().min(limit));
        for path in paths.into_iter().take(limit) {
            pending.push(read_entry(&path)?);
        }
        Ok(pending)
    }

    pub fn mark_delivered(&self, id: &str) -> io::Result<()> {
        let path = self.entry_path(id);
        match fs::remove_file(&path) {
            Ok(()) => sync_directory(&self.pending_dir),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub fn record_failure(
        &self,
        id: &str,
        error: impl Into<String>,
        retry_after: Option<Duration>,
        terminal: bool,
    ) -> io::Result<()> {
        let path = self.entry_path(id);
        let mut entry = read_entry(&path)?;
        entry.attempts = entry.attempts.saturating_add(1);
        entry.updated_at_ms = now_ms();
        entry.next_attempt_at_ms = retry_after
            .map(|delay| entry.updated_at_ms.saturating_add(delay.as_millis() as u64))
            .unwrap_or(0);
        entry.last_error = Some(error.into());
        entry.terminal = terminal;
        write_entry_atomic(&path, &entry)
    }

    fn entry_path(&self, id: &str) -> PathBuf {
        self.pending_dir.join(format!("{id}.json"))
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

    fn intent(location: &str) -> CatalogIntent {
        CatalogIntent {
            version: CATALOG_INTENT_VERSION,
            identity: CatalogIntentIdentity {
                sink_ref: "primary".into(),
                namespace: "events".into(),
                kind: CatalogIntentKind::UpsertPartition,
                key: "day=2026-07-30".into(),
            },
            payload_json: serde_json::json!({ "location": location }).to_string(),
        }
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
        let id = outbox.scan_pending(1).unwrap()[0].id.clone();
        outbox
            .record_failure(&id, "throttled", Some(Duration::from_secs(1)), false)
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
}
