use std::fs;
use std::path::Path;

use skippr_lease::{DurableError, PipelinePaths, SegmentId};

use super::mutation::{DurableMutation, MutationEnvelope};
use crate::buffer::compaction_transaction::{CompactionTransaction, CompactionTransactionState};
use crate::buffer::completion_ledger::{SegmentCompletionLedger, SegmentCompletionUpdate};
use crate::buffer::ingest_buffer::Buffers;
use crate::buffer::segment_file::SegmentFile;

pub struct DurableApplicator {
    paths: PipelinePaths,
}

pub enum ApplyMode {
    Live,
    CatchUp,
}

impl DurableApplicator {
    pub fn new(paths: PipelinePaths) -> Self {
        Self { paths }
    }

    pub fn apply(&self, envelope: &MutationEnvelope) -> Result<(), DurableError> {
        self.apply_with(envelope, ApplyMode::Live)
    }

    pub fn apply_catch_up(&self, envelope: &MutationEnvelope) -> Result<(), DurableError> {
        self.apply_with(envelope, ApplyMode::CatchUp)
    }

    fn apply_with(&self, envelope: &MutationEnvelope, mode: ApplyMode) -> Result<(), DurableError> {
        match &envelope.body {
            DurableMutation::CommitSegment { descriptor, .. } => {
                let id = SegmentId::new(&descriptor.segment_id)
                    .map_err(|err| DurableError::Io(err.to_string()))?;
                let path = self.paths.segment(&id);
                Buffers::write_seg_commit(
                    &path,
                    &descriptor.payload_sha256,
                    descriptor.num_partitions,
                    descriptor.total_bytes,
                )?;
                Ok(())
            }
            DurableMutation::PutCompaction { transaction } => {
                persist_portable_manifest(&self.paths.compactions, transaction)?;
                Buffers::planner_apply_compaction(transaction);
                Ok(())
            }
            DurableMutation::CompleteSlices { entries, .. } => {
                let ledger = SegmentCompletionLedger::new(self.paths.completions.clone());
                let mut owned: Vec<(
                    String,
                    Vec<crate::buffer::segment_file::SegmentPartitionIndexEntry>,
                    Vec<usize>,
                )> = Vec::new();
                for entry in entries {
                    let id = SegmentId::new(&entry.segment_id)
                        .map_err(|err| DurableError::Io(err.to_string()))?;
                    let seg = SegmentFile::new(&self.paths.segs, id.as_str())?;
                    let meta = match seg.read_metadata_durable() {
                        Ok(meta) => meta,
                        Err(err)
                            if matches!(mode, ApplyMode::CatchUp)
                                && err.kind() == std::io::ErrorKind::NotFound =>
                        {
                            continue;
                        }
                        Err(err) => return Err(err.into()),
                    };
                    let ordinals: Vec<usize> = entry
                        .ordinals
                        .iter()
                        .map(|ordinal| *ordinal as usize)
                        .collect();
                    owned.push((entry.segment_id.clone(), meta.index, ordinals));
                }
                let updates: Vec<SegmentCompletionUpdate<'_>> = owned
                    .iter()
                    .map(|(segment_id, index, ordinals)| SegmentCompletionUpdate {
                        segment_id,
                        index,
                        ordinals,
                    })
                    .collect();
                ledger
                    .mark_complete_batch(&updates)
                    .map_err(|err| DurableError::Io(err.to_string()))?;
                for entry in entries {
                    Buffers::planner_complete_ordinals(&entry.segment_id, &entry.ordinals);
                }
                Ok(())
            }
            DurableMutation::ReclaimSegment { segment_id } => {
                let id =
                    SegmentId::new(segment_id).map_err(|err| DurableError::Io(err.to_string()))?;
                let path = self.paths.segment(&id);
                let index = SegmentFile { path: path.clone() }
                    .read_metadata_durable()
                    .map(|meta| meta.index)
                    .unwrap_or_default();
                let _ = fs::remove_file(&path);
                let _ = fs::remove_file(self.paths.segment_commit(&id));
                let ledger = SegmentCompletionLedger::new(self.paths.completions.clone());
                let _ = ledger.remove_segment(segment_id, &index);
                Buffers::forget_reclaimed_segment(segment_id);
                Ok(())
            }
        }
    }
}

fn persist_portable_manifest(dir: &Path, txn: &CompactionTransaction) -> Result<(), DurableError> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", txn.id));
    if matches!(txn.state, CompactionTransactionState::Tombstoned) {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("json.tmp"));
        return Ok(());
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(txn).map_err(|err| DurableError::Io(err.to_string()))?;
    fs::write(&tmp, &bytes)?;
    let file = std::fs::File::open(&tmp)?;
    file.sync_all()?;
    fs::rename(&tmp, &path)?;
    #[cfg(not(windows))]
    {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::PipelineKey;

    #[test]
    fn reclaim_missing_segment_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        fs::create_dir_all(&paths.segs).unwrap();
        let applicator = DurableApplicator::new(paths);
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: skippr_lease::LeaseEpoch::new(1),
            index: skippr_lease::CommitIndex::new(1),
            previous_hash: skippr_lease::GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "missing".into(),
            },
        };
        applicator.apply(&envelope).unwrap();
    }

    #[test]
    fn complete_slices_requires_the_segment_file() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        fs::create_dir_all(&paths.segs).unwrap();
        let applicator = DurableApplicator::new(paths);
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: skippr_lease::LeaseEpoch::new(1),
            index: skippr_lease::CommitIndex::new(1),
            previous_hash: skippr_lease::GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body: DurableMutation::CompleteSlices {
                compaction_id: "c1".into(),
                entries: vec![crate::buffer::durable::mutation::CompletedOrdinals {
                    segment_id: "seg-missing".into(),
                    ordinals: vec![0],
                }],
            },
        };
        assert!(applicator.apply(&envelope).is_err());
    }

    #[test]
    fn catch_up_complete_slices_skips_reclaimed_segment() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        fs::create_dir_all(&paths.segs).unwrap();
        let applicator = DurableApplicator::new(paths);
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: skippr_lease::LeaseEpoch::new(1),
            index: skippr_lease::CommitIndex::new(1),
            previous_hash: skippr_lease::GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body: DurableMutation::CompleteSlices {
                compaction_id: "c1".into(),
                entries: vec![crate::buffer::durable::mutation::CompletedOrdinals {
                    segment_id: "seg-missing".into(),
                    ordinals: vec![0],
                }],
            },
        };
        applicator.apply_catch_up(&envelope).unwrap();
    }

    fn test_txn(id: &str, state: CompactionTransactionState) -> CompactionTransaction {
        CompactionTransaction {
            id: id.to_string(),
            sink_ref: "sink".into(),
            namespace: "ns".into(),
            schema_fingerprint: "fp".into(),
            write_policy: crate::plugins::source_contract::WritePolicy::Append,
            semantics: crate::buffer::compaction_transaction::SinkWriteSemantics::default(),
            refs: Vec::new(),
            target_filename: "out.parquet".into(),
            created_at_secs: 1,
            updated_at_secs: 1,
            attempts: 0,
            state,
        }
    }

    #[test]
    fn put_compaction_tombstone_deletes_json() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        fs::create_dir_all(&paths.compactions).unwrap();
        let applicator = DurableApplicator::new(paths.clone());
        let pending = MutationEnvelope {
            protocol_version: 1,
            pipeline: key.clone(),
            epoch: skippr_lease::LeaseEpoch::new(1),
            index: skippr_lease::CommitIndex::new(1),
            previous_hash: skippr_lease::GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body: DurableMutation::PutCompaction {
                transaction: test_txn("c1", CompactionTransactionState::Pending),
            },
        };
        applicator.apply(&pending).unwrap();
        let json = paths.compactions.join("c1.json");
        assert!(json.exists());
        let tombstone = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: skippr_lease::LeaseEpoch::new(1),
            index: skippr_lease::CommitIndex::new(2),
            previous_hash: skippr_lease::GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body: DurableMutation::PutCompaction {
                transaction: test_txn("c1", CompactionTransactionState::Tombstoned),
            },
        };
        applicator.apply(&tombstone).unwrap();
        assert!(!json.exists());
    }
}
