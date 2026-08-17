//! First clustered acquisition of an uninitialized lease installs a baseline
//! from the legacy disk WAL, then writes `CLUSTER_FORMAT_V1`.

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};
use skippr_lease::{DurableError, PipelineKey, PipelinePaths, SegmentId};

use crate::buffer::compaction_transaction::{CompactionTransaction, CompactionTransactionState};
use crate::buffer::durable::log::MutationLog;
use crate::buffer::durable::mutation::{CommittedCheckpoint, CommittedOffset, SegmentDescriptor};
use crate::buffer::durable::snapshot::{write_snapshot, StateSnapshot};
use crate::buffer::segment_file::SegmentFile;
use crate::helpers::offsets::SLED_NAME;

pub fn install_if_needed(
    key: &PipelineKey,
    clustered: &PipelinePaths,
    legacy_root: Option<&Path>,
    lease_initialized: bool,
) -> Result<StateSnapshot, DurableError> {
    let marker = clustered.format_marker().exists();
    let clustered_has_log = clustered.mutation_log().exists()
        && fs::metadata(clustered.mutation_log())
            .map(|meta| meta.len() > 0)
            .unwrap_or(false);
    let legacy_has_wal = legacy_root
        .map(|root| has_committed_segments(&root.join("segment_buffer/segs")))
        .unwrap_or(false);

    if has_live_compaction(&clustered.compactions)
        || legacy_root
            .map(|root| has_live_compaction(&root.join("segment_buffer/compactions")))
            .unwrap_or(false)
    {
        return Err(DurableError::ProtocolMismatch(
            "refusing clustered baseline while a live compaction is in flight".into(),
        ));
    }

    if clustered_has_log && legacy_has_wal && !marker {
        return Err(DurableError::ProtocolMismatch(
            "incompatible clustered and legacy WAL baselines".into(),
        ));
    }

    if lease_initialized && !marker {
        return Err(DurableError::ProtocolMismatch(
            "initialized lease without CLUSTER_FORMAT_V1; fail closed".into(),
        ));
    }

    if marker {
        return Ok(empty_snapshot(key));
    }

    fs::create_dir_all(&clustered.segs)?;
    fs::create_dir_all(&clustered.completions)?;
    fs::create_dir_all(&clustered.compactions)?;
    fs::create_dir_all(&clustered.durable)?;
    fs::create_dir_all(&clustered.snapshots)?;

    let mut snapshot = empty_snapshot(key);
    if let Some(root) = legacy_root {
        if legacy_has_wal {
            snapshot.segments = copy_legacy_segments(root, clustered)?;
            let (offsets, checkpoints) = collect_legacy_sled(root);
            snapshot.offsets = offsets;
            snapshot.checkpoints = checkpoints;
        }
    }

    write_snapshot(clustered, &snapshot)?;
    let log = MutationLog::open(clustered.clone())?;
    log.write_format_marker()?;
    Ok(snapshot)
}

fn empty_snapshot(key: &PipelineKey) -> StateSnapshot {
    StateSnapshot {
        pipeline_tenant: key.tenant().to_string(),
        pipeline_workspace: key.workspace().to_string(),
        pipeline_name: key.pipeline().to_string(),
        base_index: 0,
        base_hash: skippr_lease::GENESIS_HASH,
        segments: Vec::new(),
        compactions: Vec::new(),
        completions: Vec::new(),
        offsets: Vec::new(),
        checkpoints: Vec::new(),
        schema_fingerprints: Vec::new(),
    }
}

fn has_committed_segments(segs: &Path) -> bool {
    let Ok(entries) = fs::read_dir(segs) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.ends_with(".seg.commit"))
            .unwrap_or(false)
    })
}

fn has_live_compaction(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            return false;
        }
        fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CompactionTransaction>(&bytes).ok())
            .map(|txn| {
                matches!(
                    txn.state,
                    CompactionTransactionState::Pending | CompactionTransactionState::Sent
                )
            })
            .unwrap_or(false)
    })
}

fn copy_legacy_segments(
    legacy_root: &Path,
    clustered: &PipelinePaths,
) -> Result<Vec<SegmentDescriptor>, DurableError> {
    let segs = legacy_root.join("segment_buffer/segs");
    let mut descriptors = Vec::new();
    let Ok(entries) = fs::read_dir(&segs) else {
        return Ok(descriptors);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_string();
        if !name.ends_with(".seg") {
            continue;
        }
        let id = name.trim_end_matches(".seg");
        let commit = segs.join(format!("{id}.seg.commit"));
        if !commit.exists() {
            continue;
        }
        let dest = clustered
            .segment(&SegmentId::new(id).map_err(|err| DurableError::Io(err.to_string()))?);
        fs::copy(&path, &dest)?;
        fs::copy(
            &commit,
            clustered.segment_commit(
                &SegmentId::new(id).map_err(|err| DurableError::Io(err.to_string()))?,
            ),
        )?;
        let seg = SegmentFile::new(&clustered.segs, id)?;
        let meta = seg
            .read_metadata()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let payload = fs::read(&dest)?;
        let mut hasher = Sha256::new();
        hasher.update(&payload);
        let payload_sha256: [u8; 32] = hasher.finalize().into();
        descriptors.push(SegmentDescriptor {
            segment_id: id.to_string(),
            payload_len: meta.total_bytes,
            payload_sha256,
            num_partitions: meta.num_partitions,
            total_bytes: meta.total_bytes,
            created_at_secs: meta.created_at_secs,
            schema_fingerprints: Vec::new(),
        });
    }
    Ok(descriptors)
}

fn collect_legacy_sled(legacy_root: &Path) -> (Vec<CommittedOffset>, Vec<CommittedCheckpoint>) {
    let Ok(db) = sled::open(legacy_root.join(SLED_NAME)) else {
        return (Vec::new(), Vec::new());
    };
    let Ok(tree) = db.open_tree("offsets") else {
        return (Vec::new(), Vec::new());
    };
    let mut offsets = Vec::new();
    let mut checkpoints = Vec::new();
    for kv in tree.iter() {
        let Ok((key, value)) = kv else { continue };
        let Ok(key) = std::str::from_utf8(&key) else {
            continue;
        };
        if let Some(logical) = key.strip_prefix("cdc_checkpoint:") {
            checkpoints.push(CommittedCheckpoint {
                logical_key: logical.to_string(),
                envelope: value.to_vec(),
            });
            continue;
        }
        if key.ends_with("-latest") || value.len() != 24 {
            continue;
        }
        let Some((namespace, partition)) = key.rsplit_once('-') else {
            continue;
        };
        let filesize = u64::from_le_bytes(value[0..8].try_into().unwrap());
        let position = u64::from_le_bytes(value[8..16].try_into().unwrap());
        let closed = u64::from_le_bytes(value[16..24].try_into().unwrap());
        offsets.push(CommittedOffset {
            namespace: namespace.to_string(),
            partition: partition.to_string(),
            filesize,
            position,
            closed,
        });
    }
    (offsets, checkpoints)
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::PipelineKey;

    #[test]
    fn missing_marker_on_initialized_lease_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let err = install_if_needed(&key, &paths, None, true).unwrap_err();
        assert!(matches!(err, DurableError::ProtocolMismatch(_)));
    }

    #[test]
    fn uninitialized_existing_lease_writes_format_marker() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        install_if_needed(&key, &paths, None, false).unwrap();
        assert!(paths.format_marker().exists());
    }
}
