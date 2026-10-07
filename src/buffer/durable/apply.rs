use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use skippr_lease::{DurableError, PipelinePaths, SegmentId};

use super::mutation::{DurableMutation, MutationEnvelope};
use crate::buffer::compaction_transaction::{CompactionTransaction, CompactionTransactionState};
use crate::buffer::completion_ledger::{SegmentCompletionLedger, SegmentCompletionUpdate};
use crate::buffer::ingest_buffer::Buffers;
use crate::buffer::segment_file::{SegmentFile, SegmentFileMetadata};

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
                let mut by_segment: BTreeMap<&str, BTreeSet<usize>> = BTreeMap::new();
                for entry in entries {
                    by_segment
                        .entry(entry.segment_id.as_str())
                        .or_default()
                        .extend(entry.ordinals.iter().map(|ordinal| *ordinal as usize));
                }
                let mut owned: Vec<(&str, Arc<SegmentFileMetadata>, Vec<usize>)> =
                    Vec::with_capacity(by_segment.len());
                for (segment_id, ordinals) in by_segment {
                    let id = SegmentId::new(segment_id)
                        .map_err(|err| DurableError::Io(err.to_string()))?;
                    let meta = match Buffers::segment_index(&self.paths.segment(&id)) {
                        Ok(meta) => meta,
                        Err(err)
                            if matches!(mode, ApplyMode::CatchUp)
                                && err.kind() == std::io::ErrorKind::NotFound =>
                        {
                            continue;
                        }
                        Err(err) => return Err(err.into()),
                    };
                    owned.push((segment_id, meta, ordinals.into_iter().collect()));
                }
                let updates: Vec<SegmentCompletionUpdate<'_>> = owned
                    .iter()
                    .map(|(segment_id, meta, ordinals)| SegmentCompletionUpdate {
                        segment_id,
                        index: &meta.index,
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
                let meta = Buffers::segment_index(&path).ok();
                let index = meta.as_ref().map_or(&[][..], |meta| meta.index.as_slice());
                SegmentFile::reclaim_local_pair(&path)
                    .map_err(|err| DurableError::Io(err.to_string()))?;
                let ledger = SegmentCompletionLedger::new(self.paths.completions.clone());
                ledger
                    .remove_segment(segment_id, index)
                    .map_err(|err| DurableError::Io(err.to_string()))?;
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
    crate::helpers::fsync::fsync_dir(dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::{PipelineKey, SegmentId};

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
    fn reclaim_does_not_succeed_if_body_drop_fails_after_unown() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        fs::create_dir_all(&paths.segs).unwrap();
        let id = SegmentId::new("stuck").unwrap();
        let seg = paths.segment(&id);
        fs::create_dir(&seg).unwrap();
        fs::write(paths.segment_commit(&id), b"SEGC").unwrap();
        let applicator = DurableApplicator::new(paths.clone());
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: skippr_lease::LeaseEpoch::new(1),
            index: skippr_lease::CommitIndex::new(1),
            previous_hash: skippr_lease::GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "stuck".into(),
            },
        };
        assert!(applicator.apply(&envelope).is_err());
        assert!(!paths.segment_commit(&id).exists());
        assert!(seg.exists());
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

    fn envelope(key: &PipelineKey, index: u64, body: DurableMutation) -> MutationEnvelope {
        MutationEnvelope {
            protocol_version: 1,
            pipeline: key.clone(),
            epoch: skippr_lease::LeaseEpoch::new(1),
            index: skippr_lease::CommitIndex::new(index),
            previous_hash: skippr_lease::GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body,
        }
    }

    fn committed_two_part_segment(paths: &PipelinePaths, name: &str) -> std::path::PathBuf {
        use crate::buffer::segment_file::PartitionKey;
        use arrow::array::{Int32Array, RecordBatch};
        use arrow::datatypes::{DataType, Field, Schema};
        use std::collections::HashMap;
        use std::sync::Arc;

        let seg = SegmentFile::new(&paths.segs, name).unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int32, false)]));
        let mut batches = HashMap::new();
        let mut parts_meta = HashMap::new();
        for part in 0..2 {
            let key = PartitionKey {
                sink_ref: "out".into(),
                namespace: "ns".into(),
                partition: format!("p{part}"),
                time: Some(0),
                schema_fingerprint: "schema".into(),
            };
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(Int32Array::from(vec![1, 2, 3]))],
            )
            .unwrap();
            batches.insert(key.clone(), vec![batch]);
            parts_meta.insert(key, (0, std::time::SystemTime::UNIX_EPOCH));
        }
        let (meta, _rows, sha) = seg
            .write_snapshot(&HashMap::new(), &batches, &parts_meta, &HashMap::new())
            .unwrap();
        Buffers::write_seg_commit(&seg.path, &sha, meta.num_partitions, meta.total_bytes).unwrap();
        seg.path
    }

    #[test]
    fn complete_and_reclaim_read_the_segment_index_once_without_hashing_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        fs::create_dir_all(&paths.segs).unwrap();
        let path = committed_two_part_segment(&paths, "seg-index-only");
        let meta = SegmentFile::read_index_path(&path).unwrap();
        let mut body = fs::read(&path).unwrap();
        body[meta.index[0].start as usize + 1] ^= 0xff;
        fs::write(&path, &body).unwrap();
        assert!(
            SegmentFile::admit_owned_pair_path(&path).is_err(),
            "startup reconcile and catch-up admission must still reject the payload"
        );

        let applicator = DurableApplicator::new(paths.clone());
        let reads_before = SegmentFile::index_read_count();
        let entries = (0..16u32)
            .map(|i| crate::buffer::durable::mutation::CompletedOrdinals {
                segment_id: "seg-index-only".into(),
                ordinals: vec![i % 2],
            })
            .collect();
        applicator
            .apply(&envelope(
                &key,
                1,
                DurableMutation::CompleteSlices {
                    compaction_id: "c1".into(),
                    entries,
                },
            ))
            .unwrap();
        assert_eq!(SegmentFile::index_read_count() - reads_before, 1);
        let ledger = SegmentCompletionLedger::new(paths.completions.clone());
        assert!(ledger.all_complete("seg-index-only", &meta.index).unwrap());

        applicator
            .apply(&envelope(
                &key,
                2,
                DurableMutation::ReclaimSegment {
                    segment_id: "seg-index-only".into(),
                },
            ))
            .unwrap();
        assert!(!path.exists());
        assert!(!ledger.bitmap_path("seg-index-only").exists());
        for entry in &meta.index {
            assert!(!ledger
                .legacy_tombstone_path("seg-index-only", &entry.key)
                .exists());
        }
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
