use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use skippr_lease::{CommitIndex, DurableError, PipelineKey, PipelinePaths, GENESIS_HASH};

use super::log::MutationLog;
use super::mutation::{CommittedCheckpoint, CommittedOffset, DurableMutation, SegmentDescriptor};
use crate::buffer::compaction_transaction::{CompactionTransaction, CompactionTransactionState};
use crate::buffer::direct_io::DirectIoFile;

const PACK_MAGIC: &[u8; 12] = b"SKIPPRSNAP1\n";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub pipeline_tenant: String,
    pub pipeline_workspace: String,
    pub pipeline_name: String,
    pub base_index: u64,
    pub base_hash: [u8; 32],
    pub segments: Vec<SegmentDescriptor>,
    pub compactions: Vec<CompactionTransaction>,
    pub completions: Vec<CompletionSnapshot>,
    pub offsets: Vec<CommittedOffset>,
    pub checkpoints: Vec<CommittedCheckpoint>,
    pub schema_fingerprints: Vec<String>,
}

impl StateSnapshot {
    pub fn pipeline(&self) -> Result<PipelineKey, DurableError> {
        PipelineKey::new(
            &self.pipeline_tenant,
            &self.pipeline_workspace,
            &self.pipeline_name,
        )
        .map_err(|err| DurableError::Io(err.to_string()))
    }

    pub fn base_index(&self) -> CommitIndex {
        CommitIndex::new(self.base_index)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompletionSnapshot {
    pub segment_id: String,
    pub bitmap: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotInstallPhase {
    Staging,
    QuarantineRename,
    Live,
}

pub fn snapshot_staging_dir(root: &Path, uuid: &str) -> PathBuf {
    root.parent().unwrap_or(root).join(format!(
        "{}.bootstrap-{uuid}",
        root.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("pipeline")
    ))
}

/// Retained snapshot = metadata only. Live payloads stay in place under the
/// pipeline root; [`write_transfer_pack`] materializes them for a replica.
pub fn write_snapshot(
    paths: &PipelinePaths,
    snapshot: &StateSnapshot,
) -> Result<PathBuf, DurableError> {
    fs::create_dir_all(&paths.snapshots)?;
    let json = serde_json::to_vec(snapshot).map_err(|err| DurableError::Io(err.to_string()))?;
    let digest: [u8; 32] = Sha256::digest(&json).into();
    let mut bytes = Vec::with_capacity(4 + json.len() + digest.len());
    bytes.extend_from_slice(&(json.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&json);
    bytes.extend_from_slice(&digest);
    let staging = paths.snapshots.join("current.tmp");
    DirectIoFile::write_path_sync(&staging, &bytes)?;
    let dest = paths.snapshot_current();
    fs::rename(&staging, &dest)?;
    crate::helpers::fsync::fsync_dir(&paths.snapshots)?;
    Ok(dest)
}

pub fn read_snapshot(paths: &PipelinePaths) -> Result<Option<StateSnapshot>, DurableError> {
    let path = paths.snapshot_current();
    if !path.exists() {
        return Ok(None);
    }
    let mut file = fs::File::open(&path)?;
    let mut magic = [0u8; PACK_MAGIC.len()];
    if file.read_exact(&mut magic).is_ok() && &magic == PACK_MAGIC {
        return read_pack_meta(&mut file).map(Some);
    }
    drop(file);
    let bytes = DirectIoFile::read_path(&path)?;
    if bytes.len() < 4 + 32 {
        return Err(DurableError::Io("corrupt snapshot".into()));
    }
    let len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    if bytes.len() != 4 + len + 32 {
        return Err(DurableError::Io("corrupt snapshot length".into()));
    }
    let json = &bytes[4..4 + len];
    let expected = &bytes[4 + len..];
    let mut hasher = Sha256::new();
    hasher.update(json);
    let actual: [u8; 32] = hasher.finalize().into();
    if actual.as_slice() != expected {
        return Err(DurableError::Io("snapshot checksum mismatch".into()));
    }
    serde_json::from_slice(json)
        .map(Some)
        .map_err(|err| DurableError::Io(err.to_string()))
}

/// Compaction planner and log retention share this snapshot+suffix-log view.
pub fn clustered_compaction_sot(
    paths: &PipelinePaths,
    log: &MutationLog,
    key: &PipelineKey,
) -> Result<StateSnapshot, DurableError> {
    build_live_snapshot(paths, log, key)
}

pub fn build_live_snapshot(
    paths: &PipelinePaths,
    log: &MutationLog,
    key: &PipelineKey,
) -> Result<StateSnapshot, DurableError> {
    let mut segments: BTreeMap<String, SegmentDescriptor> = BTreeMap::new();
    let mut offsets: BTreeMap<(String, String), CommittedOffset> = BTreeMap::new();
    let mut checkpoints: BTreeMap<String, CommittedCheckpoint> = BTreeMap::new();
    let mut compactions: BTreeMap<String, CompactionTransaction> = BTreeMap::new();
    let mut fingerprints = Vec::new();
    if let Some(base) = read_snapshot(paths)? {
        if base.base_index == log.base_index().get() {
            for descriptor in base.segments {
                segments.insert(descriptor.segment_id.clone(), descriptor);
            }
            for offset in base.offsets {
                offsets.insert((offset.namespace.clone(), offset.partition.clone()), offset);
            }
            for checkpoint in base.checkpoints {
                checkpoints.insert(checkpoint.logical_key.clone(), checkpoint);
            }
            for transaction in base.compactions {
                if !matches!(transaction.state, CompactionTransactionState::Tombstoned) {
                    compactions.insert(transaction.id.clone(), transaction);
                }
            }
            fingerprints = base.schema_fingerprints;
        }
    }
    for envelope in log.committed_envelopes() {
        match &envelope.body {
            DurableMutation::CommitSegment {
                descriptor,
                offsets: committed,
                checkpoints: cps,
            } => {
                let id = skippr_lease::SegmentId::new(&descriptor.segment_id)
                    .map_err(|err| DurableError::Io(err.to_string()))?;
                fingerprints.extend(descriptor.schema_fingerprints.iter().cloned());
                if paths.segment(&id).exists() {
                    segments.insert(descriptor.segment_id.clone(), descriptor.clone());
                } else {
                    segments.remove(&descriptor.segment_id);
                }
                for offset in committed {
                    offsets.insert(
                        (offset.namespace.clone(), offset.partition.clone()),
                        offset.clone(),
                    );
                }
                for checkpoint in cps {
                    checkpoints.insert(checkpoint.logical_key.clone(), checkpoint.clone());
                }
            }
            DurableMutation::PutCompaction { transaction } => {
                if matches!(transaction.state, CompactionTransactionState::Tombstoned) {
                    compactions.remove(&transaction.id);
                } else {
                    compactions.insert(transaction.id.clone(), transaction.clone());
                }
            }
            DurableMutation::ReclaimSegment { segment_id } => {
                segments.remove(segment_id);
            }
            DurableMutation::CompleteSlices { .. } => {}
        }
    }
    segments.retain(|segment_id, _| {
        skippr_lease::SegmentId::new(segment_id)
            .map(|id| paths.segment(&id).exists())
            .unwrap_or(false)
    });
    let live: HashSet<String> = segments.keys().cloned().collect();
    compactions.retain(|_, txn| {
        !matches!(txn.state, CompactionTransactionState::Tombstoned)
            && txn
                .refs
                .iter()
                .all(|wal_ref| live.contains(&wal_ref.segment_id))
    });
    fingerprints.sort();
    fingerprints.dedup();
    Ok(StateSnapshot {
        pipeline_tenant: key.tenant().to_string(),
        pipeline_workspace: key.workspace().to_string(),
        pipeline_name: key.pipeline().to_string(),
        base_index: log.committed_index().get(),
        base_hash: log.head_hash(),
        segments: segments.into_values().collect(),
        compactions: compactions.into_values().collect(),
        completions: collect_completions(paths),
        offsets: offsets.into_values().collect(),
        checkpoints: checkpoints.into_values().collect(),
        schema_fingerprints: fingerprints,
    })
}

/// Persist live metadata, then prune the mutation log through the committed
/// head. Runs on every segment reclaim, so it must stay O(metadata) — never
/// O(live WAL bytes).
pub fn retain_live_snapshot(
    log: &mut MutationLog,
    paths: &PipelinePaths,
    key: &PipelineKey,
) -> Result<StateSnapshot, DurableError> {
    let snapshot = build_live_snapshot(paths, log, key)?;
    let live_ids: HashSet<String> = snapshot
        .compactions
        .iter()
        .map(|txn| txn.id.clone())
        .collect();
    prune_orphan_compaction_json(paths, &live_ids);
    write_snapshot(paths, &snapshot)?;
    log.prune_through(log.committed_index())?;
    tracing::info!(
        pipeline = %key.pipeline(),
        base = snapshot.base_index,
        live_segments = snapshot.segments.len(),
        "clustered snapshot retained"
    );
    Ok(snapshot)
}

#[derive(Debug)]
pub struct TransferPack {
    pub path: PathBuf,
    pub base_index: u64,
    pub base_hash: [u8; 32],
}

/// Pack live payloads + metadata at the committed head for a replica that is
/// behind the retained base. The caller holds the log lock for the whole copy
/// so reclaim cannot delete a payload the pack metadata still lists.
pub fn write_transfer_pack(
    paths: &PipelinePaths,
    log: &MutationLog,
    key: &PipelineKey,
) -> Result<TransferPack, DurableError> {
    let snapshot = build_live_snapshot(paths, log, key)?;
    fs::create_dir_all(&paths.snapshots)?;
    let path = paths
        .snapshots
        .join(format!("transfer-{}.pack", uuid::Uuid::new_v4()));
    let written = write_pack(&path, paths, &snapshot);
    if written.is_err() {
        let _ = fs::remove_file(&path);
    }
    written?;
    Ok(TransferPack {
        path,
        base_index: snapshot.base_index,
        base_hash: snapshot.base_hash,
    })
}

fn write_pack(
    dest: &Path,
    paths: &PipelinePaths,
    snapshot: &StateSnapshot,
) -> Result<(), DurableError> {
    let json = serde_json::to_vec(snapshot).map_err(|err| DurableError::Io(err.to_string()))?;
    let mut file = DirectIoFile::create(dest)?;
    file.write_all(PACK_MAGIC)?;
    file.write_all(&(json.len() as u32).to_le_bytes())?;
    file.write_all(&json)?;
    let files = collect_live_files(paths)?;
    file.write_all(&(files.len() as u32).to_le_bytes())?;
    for (rel, abs) in files {
        let bytes = DirectIoFile::read_path(&abs)?;
        let rel_bytes = rel.as_bytes();
        file.write_all(&(rel_bytes.len() as u16).to_le_bytes())?;
        file.write_all(rel_bytes)?;
        file.write_all(&(bytes.len() as u64).to_le_bytes())?;
        file.write_all(&bytes)?;
    }
    file.sync_data()?;
    Ok(())
}

fn prune_orphan_compaction_json(paths: &PipelinePaths, live_ids: &HashSet<String>) {
    let Ok(entries) = fs::read_dir(&paths.compactions) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !live_ids.contains(id) {
            let _ = fs::remove_file(&path);
        }
    }
}

fn collect_live_files(paths: &PipelinePaths) -> Result<Vec<(String, PathBuf)>, DurableError> {
    let mut out = Vec::new();
    collect_dir_files(&paths.root, &paths.segs, &mut out)?;
    collect_dir_files(&paths.root, &paths.completions, &mut out)?;
    collect_dir_files(&paths.root, &paths.compactions, &mut out)?;
    if paths.format_marker().exists() {
        out.push((
            rel_path(&paths.root, &paths.format_marker())?,
            paths.format_marker(),
        ));
    }
    Ok(out)
}

fn collect_completions(paths: &PipelinePaths) -> Vec<CompletionSnapshot> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&paths.completions) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(segment_id) = name.strip_suffix(".seg.completion-bitmap") else {
            continue;
        };
        let Ok(bitmap) = fs::read(&path) else {
            continue;
        };
        out.push(CompletionSnapshot {
            segment_id: segment_id.to_string(),
            bitmap,
        });
    }
    out
}

fn collect_dir_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), DurableError> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            out.push((rel_path(root, &path)?, path));
        }
    }
    Ok(())
}

fn rel_path(root: &Path, path: &Path) -> Result<String, DurableError> {
    let rel = path
        .strip_prefix(root)
        .map_err(|err| DurableError::Io(err.to_string()))?;
    let text = rel.to_string_lossy().replace('\\', "/");
    if text.is_empty() || text.starts_with('/') || text.split('/').any(|part| part == "..") {
        return Err(DurableError::Io(format!("unsafe snapshot path {text}")));
    }
    Ok(text)
}

fn read_pack_meta(file: &mut fs::File) -> Result<StateSnapshot, DurableError> {
    let mut len = [0u8; 4];
    file.read_exact(&mut len)
        .map_err(|_| DurableError::Io("truncated snapshot pack".into()))?;
    let mut json = vec![0u8; u32::from_le_bytes(len) as usize];
    file.read_exact(&mut json)
        .map_err(|_| DurableError::Io("truncated snapshot json".into()))?;
    serde_json::from_slice(&json).map_err(|err| DurableError::Io(err.to_string()))
}

fn unpack_pack(bytes: &[u8], staging_root: &Path) -> Result<StateSnapshot, DurableError> {
    if !bytes.starts_with(PACK_MAGIC) {
        return Err(DurableError::Io("snapshot pack magic mismatch".into()));
    }
    let mut offset = PACK_MAGIC.len();
    let json_len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
    offset += 4;
    let json = &bytes[offset..offset + json_len];
    offset += json_len;
    let snapshot: StateSnapshot =
        serde_json::from_slice(json).map_err(|err| DurableError::Io(err.to_string()))?;
    let file_count = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
    offset += 4;
    for _ in 0..file_count {
        let path_len = u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) as usize;
        offset += 2;
        let rel = std::str::from_utf8(&bytes[offset..offset + path_len])
            .map_err(|err| DurableError::Io(err.to_string()))?;
        offset += path_len;
        if rel.is_empty() || rel.starts_with('/') || rel.contains("..") {
            return Err(DurableError::Io(format!("unsafe pack path {rel}")));
        }
        let data_len = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize;
        offset += 8;
        let data = &bytes[offset..offset + data_len];
        offset += data_len;
        let dest = staging_root.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        DirectIoFile::write_path_sync(&dest, data)?;
    }
    write_installed_state(staging_root, &snapshot)?;
    Ok(snapshot)
}

fn write_installed_state(root: &Path, snapshot: &StateSnapshot) -> Result<(), DurableError> {
    let durable = root.join("segment_buffer/durable");
    fs::create_dir_all(&durable)?;
    let mut bytes = Vec::with_capacity(56);
    bytes.extend_from_slice(&snapshot.base_index.to_le_bytes());
    bytes.extend_from_slice(&snapshot.base_index.to_le_bytes());
    bytes.extend_from_slice(&snapshot.base_index.to_le_bytes());
    bytes.extend_from_slice(&snapshot.base_hash);
    let state_path = durable.join("STATE");
    let tmp = state_path.with_extension("tmp");
    DirectIoFile::write_path_sync(&tmp, &bytes)?;
    fs::rename(&tmp, &state_path)?;
    DirectIoFile::write_path_sync(&durable.join("mutation.log"), &[])?;
    crate::helpers::fsync::fsync_dir(&durable)?;
    let marker = durable.join("CLUSTER_FORMAT_V1");
    if !marker.exists() {
        fs::write(&marker, super::mutation::CLUSTER_FORMAT_V1.as_bytes())?;
    }
    Ok(())
}

pub fn is_live_snapshot_pack(bytes: &[u8]) -> bool {
    bytes.starts_with(PACK_MAGIC) && bytes.len() > PACK_MAGIC.len() + 8
}

pub async fn install_snapshot_from_stream(
    live: &PipelinePaths,
    uuid: &str,
    snapshot_bytes: &[u8],
) -> Result<SnapshotInstallPhase, DurableError> {
    if !is_live_snapshot_pack(snapshot_bytes) {
        return Err(DurableError::Io(
            "snapshot pack required (empty or metadata-only payload refused)".into(),
        ));
    }
    let staging_root = snapshot_staging_dir(&live.root, uuid);
    if staging_root.exists() {
        fs::remove_dir_all(&staging_root)?;
    }
    fs::create_dir_all(&staging_root)?;
    let snapshot = unpack_pack(snapshot_bytes, &staging_root)?;
    tracing::info!(
        base = snapshot.base_index,
        live_segments = snapshot.segments.len(),
        "clustered snapshot installed"
    );
    let staging_paths = PipelinePaths {
        segs: staging_root.join("segment_buffer/segs"),
        completions: staging_root.join("segment_buffer/done"),
        compactions: staging_root.join("segment_buffer/compactions"),
        durable: staging_root.join("segment_buffer/durable"),
        snapshots: staging_root.join("segment_buffer/durable/snapshots"),
        root: staging_root.clone(),
    };
    write_snapshot(&staging_paths, &snapshot)?;
    let quarantine = live.root.with_extension("quarantine");
    if live.root.exists() {
        fs::rename(&live.root, &quarantine)?;
    }
    fs::rename(&staging_root, &live.root)?;
    if quarantine.exists() {
        let _ = fs::remove_dir_all(&quarantine);
    }
    crate::metrics::counters::add_cluster_snapshot_install(1);
    Ok(SnapshotInstallPhase::Live)
}

pub fn empty_snapshot_meta(key: &PipelineKey) -> StateSnapshot {
    StateSnapshot {
        pipeline_tenant: key.tenant().to_string(),
        pipeline_workspace: key.workspace().to_string(),
        pipeline_name: key.pipeline().to_string(),
        base_index: 0,
        base_hash: GENESIS_HASH,
        segments: Vec::new(),
        compactions: Vec::new(),
        completions: Vec::new(),
        offsets: Vec::new(),
        checkpoints: Vec::new(),
        schema_fingerprints: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::{DurableError, LeaseEpoch, PipelineKey};
    use std::path::Path;

    use crate::buffer::durable::log::MutationLog;
    use crate::buffer::durable::mutation::{DurableMutation, MutationEnvelope};

    #[test]
    fn staging_dir_is_sibling_not_inside_live() {
        let live = Path::new("/data/clustered/t/w/p");
        let staging = snapshot_staging_dir(live, "abc");
        assert_eq!(staging, Path::new("/data/clustered/t/w/p.bootstrap-abc"));
    }

    #[test]
    fn rel_path_allows_empty_partition_dotdot_in_filename() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let name = "seg.data_sinks.lake.shop..0.fp.tombstone";
        let path = root.join(name);
        std::fs::write(&path, b"x").unwrap();
        assert_eq!(rel_path(root, &path).unwrap(), name);
        let traversal = root.join("..").join("outside");
        assert!(rel_path(root, &traversal).is_err());
    }

    #[test]
    fn snapshot_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let snapshot = StateSnapshot {
            pipeline_tenant: "t".into(),
            pipeline_workspace: "w".into(),
            pipeline_name: "p".into(),
            base_index: 3,
            base_hash: GENESIS_HASH,
            segments: Vec::new(),
            compactions: Vec::new(),
            completions: Vec::new(),
            offsets: Vec::new(),
            checkpoints: Vec::new(),
            schema_fingerprints: vec!["fp".into()],
        };
        write_snapshot(&paths, &snapshot).unwrap();
        let loaded = read_snapshot(&paths).unwrap().unwrap();
        assert_eq!(loaded.base_index, 3);
        assert_eq!(loaded.schema_fingerprints, vec!["fp".to_string()]);
    }

    fn append_reclaim(log: &mut MutationLog, key: &PipelineKey, segment_id: &str) {
        append_body(
            log,
            key,
            DurableMutation::ReclaimSegment {
                segment_id: segment_id.into(),
            },
        );
    }

    #[test]
    fn retain_prunes_log_without_copying_segment_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        fs::create_dir_all(&paths.segs).unwrap();
        let id = skippr_lease::SegmentId::new("live-seg").unwrap();
        let payload = vec![7u8; 1 << 20];
        fs::write(paths.segment(&id), &payload).unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        append_body(
            &mut log,
            &key,
            DurableMutation::CommitSegment {
                descriptor: test_descriptor("live-seg"),
                offsets: Vec::new(),
                checkpoints: Vec::new(),
            },
        );
        append_reclaim(&mut log, &key, "other");

        let retained = retain_live_snapshot(&mut log, &paths, &key).unwrap();

        assert!(log.committed_envelopes().is_empty());
        assert_eq!(retained.segments.len(), 1);
        let on_disk = fs::metadata(paths.snapshot_current()).unwrap().len();
        assert!(
            on_disk < payload.len() as u64,
            "retained snapshot copied segment payloads ({on_disk} bytes)"
        );
        let loaded = read_snapshot(&paths).unwrap().unwrap();
        assert_eq!(loaded.base_index, log.committed_index().get());
        assert_eq!(loaded.segments[0].segment_id, "live-seg");
    }

    #[tokio::test]
    async fn transfer_pack_at_head_installs_live_payload() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        fs::create_dir_all(&paths.segs).unwrap();
        let retained_id = skippr_lease::SegmentId::new("retained-seg").unwrap();
        fs::write(paths.segment(&retained_id), b"retained-bytes").unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        append_body(
            &mut log,
            &key,
            DurableMutation::CommitSegment {
                descriptor: test_descriptor("retained-seg"),
                offsets: Vec::new(),
                checkpoints: Vec::new(),
            },
        );
        append_reclaim(&mut log, &key, "other");
        retain_live_snapshot(&mut log, &paths, &key).unwrap();
        let suffix_id = skippr_lease::SegmentId::new("suffix-seg").unwrap();
        fs::write(paths.segment(&suffix_id), b"suffix-bytes").unwrap();
        append_body(
            &mut log,
            &key,
            DurableMutation::CommitSegment {
                descriptor: test_descriptor("suffix-seg"),
                offsets: Vec::new(),
                checkpoints: Vec::new(),
            },
        );
        let head = log.committed_index().get();

        let pack = write_transfer_pack(&paths, &log, &key).unwrap();

        assert_eq!(pack.base_index, head);
        assert_eq!(pack.base_hash, log.head_hash());
        let bytes = fs::read(&pack.path).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let dest_paths = PipelinePaths::new(dest.path(), &key).unwrap();
        install_snapshot_from_stream(&dest_paths, "inst", &bytes)
            .await
            .unwrap();
        assert_eq!(
            fs::read(dest_paths.segment(&retained_id)).unwrap(),
            b"retained-bytes"
        );
        assert_eq!(
            fs::read(dest_paths.segment(&suffix_id)).unwrap(),
            b"suffix-bytes"
        );
        let reopened = MutationLog::open(dest_paths.clone()).unwrap();
        assert_eq!(reopened.committed_index().get(), head);
        assert_eq!(reopened.base_index().get(), head);
        assert!(reopened.committed_envelopes().is_empty());
        let installed = read_snapshot(&dest_paths).unwrap().unwrap();
        assert_eq!(installed.base_index, head);
        assert_eq!(installed.segments.len(), 2);
        assert!(
            fs::metadata(dest_paths.snapshot_current()).unwrap().len() < bytes.len() as u64,
            "installed snapshot kept the transfer payload"
        );
    }

    #[test]
    fn read_snapshot_accepts_pack_retained_by_previous_release() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        fs::create_dir_all(&paths.segs).unwrap();
        let id = skippr_lease::SegmentId::new("live-seg").unwrap();
        fs::write(paths.segment(&id), b"seg!").unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        append_body(
            &mut log,
            &key,
            DurableMutation::CommitSegment {
                descriptor: test_descriptor("live-seg"),
                offsets: Vec::new(),
                checkpoints: Vec::new(),
            },
        );
        let pack = write_transfer_pack(&paths, &log, &key).unwrap();
        fs::rename(&pack.path, paths.snapshot_current()).unwrap();

        let loaded = read_snapshot(&paths).unwrap().unwrap();

        assert_eq!(loaded.segments[0].segment_id, "live-seg");
    }

    #[tokio::test]
    async fn empty_or_json_snapshot_bytes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let err = install_snapshot_from_stream(&paths, "empty", &[])
            .await
            .unwrap_err();
        assert!(matches!(err, DurableError::Io(_)));
        let json = serde_json::to_vec(&empty_snapshot_meta(&key)).unwrap();
        let err = install_snapshot_from_stream(&paths, "json", &json)
            .await
            .unwrap_err();
        assert!(matches!(err, DurableError::Io(_)));
    }

    fn append_body(log: &mut MutationLog, key: &PipelineKey, body: DurableMutation) {
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key.clone(),
            epoch: LeaseEpoch::new(1),
            index: log.committed_index().next().unwrap(),
            previous_hash: log.head_hash(),
            payload_sha256: [0u8; 32],
            body,
        };
        log.append_prepared(&envelope).unwrap();
        log.append_committed(envelope.index, envelope.entry_hash().unwrap())
            .unwrap();
    }

    fn test_txn(id: &str, segment_id: &str) -> CompactionTransaction {
        CompactionTransaction {
            id: id.to_string(),
            sink_ref: "sink".into(),
            namespace: "ns".into(),
            schema_fingerprint: "fp".into(),
            write_policy: crate::plugins::source_contract::WritePolicy::Append,
            semantics: crate::buffer::compaction_transaction::SinkWriteSemantics::default(),
            refs: vec![crate::buffer::compaction_transaction::WalPartRef {
                segment_id: segment_id.to_string(),
                source: crate::buffer::compaction_transaction::SegmentSourceDescriptor::Disk {
                    path: std::path::PathBuf::from("/tmp/x.seg"),
                },
                start: 0,
                len: 1,
                key: crate::buffer::segment_file::PartitionKey {
                    sink_ref: "sink".into(),
                    namespace: "ns".into(),
                    partition: "p".into(),
                    time: None,
                    schema_fingerprint: "fp".into(),
                },
                cdc_meta_hash: None,
            }],
            target_filename: "out.parquet".into(),
            created_at_secs: 1,
            updated_at_secs: 1,
            attempts: 0,
            state: CompactionTransactionState::Pending,
        }
    }

    fn test_descriptor(segment_id: &str) -> SegmentDescriptor {
        SegmentDescriptor {
            segment_id: segment_id.into(),
            payload_len: 4,
            payload_sha256: [0u8; 32],
            num_partitions: 1,
            total_bytes: 4,
            created_at_secs: 1,
            schema_fingerprints: vec!["fp".into()],
        }
    }

    #[test]
    fn put_compaction_without_live_segment_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        fs::create_dir_all(&paths.segs).unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        append_body(
            &mut log,
            &key,
            DurableMutation::CommitSegment {
                descriptor: test_descriptor("gone-seg"),
                offsets: Vec::new(),
                checkpoints: Vec::new(),
            },
        );
        append_body(
            &mut log,
            &key,
            DurableMutation::PutCompaction {
                transaction: test_txn("c-dead", "gone-seg"),
            },
        );
        let snapshot = clustered_compaction_sot(&paths, &log, &key).unwrap();
        assert!(snapshot.segments.is_empty());
        assert!(snapshot.compactions.is_empty());
    }

    #[test]
    fn prune_and_planner_share_snapshot_plus_suffix_log() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        fs::create_dir_all(&paths.segs).unwrap();
        fs::create_dir_all(&paths.compactions).unwrap();
        let id = skippr_lease::SegmentId::new("live-seg").unwrap();
        fs::write(paths.segment(&id), b"seg!").unwrap();
        fs::write(paths.compactions.join("c-live.json"), b"{}").unwrap();
        fs::write(paths.compactions.join("c-dead.json"), b"{}").unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        append_body(
            &mut log,
            &key,
            DurableMutation::CommitSegment {
                descriptor: test_descriptor("live-seg"),
                offsets: Vec::new(),
                checkpoints: Vec::new(),
            },
        );
        append_body(
            &mut log,
            &key,
            DurableMutation::PutCompaction {
                transaction: test_txn("c-live", "live-seg"),
            },
        );
        let retained = retain_live_snapshot(&mut log, &paths, &key).unwrap();
        assert_eq!(retained.compactions.len(), 1);
        assert_eq!(retained.compactions[0].id, "c-live");
        assert!(log.committed_envelopes().is_empty());
        assert!(!paths.compactions.join("c-dead.json").exists());
        assert!(paths.compactions.join("c-live.json").exists());

        let from_snapshot = clustered_compaction_sot(&paths, &log, &key).unwrap();
        assert_eq!(from_snapshot.segments.len(), 1);
        assert_eq!(from_snapshot.compactions.len(), 1);

        append_body(
            &mut log,
            &key,
            DurableMutation::ReclaimSegment {
                segment_id: "live-seg".into(),
            },
        );
        crate::buffer::segment_file::SegmentFile::reclaim_local_pair(&paths.segment(&id)).unwrap();
        let after_reclaim = clustered_compaction_sot(&paths, &log, &key).unwrap();
        assert!(after_reclaim.segments.is_empty());
        assert!(after_reclaim.compactions.is_empty());
    }
}
