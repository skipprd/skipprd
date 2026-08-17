use std::collections::HashSet;

use skippr_lease::{PipelineKey, SegmentId};

use crate::buffer::durable::log::MutationLog;
use crate::buffer::durable::mutation::DurableMutation;
use crate::buffer::segment_file::SegmentFile;

#[derive(Clone, Debug)]
pub struct LiveWalScanRequest {
    pub pipeline: PipelineKey,
    pub namespace: String,
    pub exclude_segment_ids: Vec<String>,
}

/// Live incomplete ordinals: snapshot segments ∪ suffix `CommitSegment`, minus
/// `ReclaimSegment`, minus Iceberg `skippr.wal-segment-ids`. Unreadable
/// ordinals are skipped. Ledger-complete ordinals stay visible until Iceberg
/// lists the segment (Sent is not lake-visible).
pub fn select_live_ordinals(
    log: &MutationLog,
    exclude_segment_ids: &[String],
) -> Vec<(String, u32)> {
    let exclude: HashSet<&str> = exclude_segment_ids.iter().map(String::as_str).collect();
    let mut live: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let mut reclaimed = HashSet::new();
    if let Ok(Some(snapshot)) = crate::buffer::durable::snapshot::read_snapshot(log.paths()) {
        for descriptor in snapshot.segments {
            if seen.insert(descriptor.segment_id.clone()) {
                live.push(descriptor.segment_id);
            }
        }
    }
    for envelope in log.committed_envelopes() {
        match &envelope.body {
            DurableMutation::CommitSegment { descriptor, .. } => {
                if seen.insert(descriptor.segment_id.clone()) {
                    live.push(descriptor.segment_id.clone());
                }
            }
            DurableMutation::ReclaimSegment { segment_id } => {
                reclaimed.insert(segment_id.clone());
            }
            DurableMutation::PutCompaction { .. } | DurableMutation::CompleteSlices { .. } => {}
        }
    }
    live.retain(|id| !reclaimed.contains(id) && !exclude.contains(id.as_str()));

    let mut out = Vec::new();
    for id in live {
        let Ok(seg_id) = SegmentId::new(&id) else {
            continue;
        };
        let path = log.paths().segment(&seg_id);
        let Ok(meta) = (SegmentFile { path }).read_metadata() else {
            continue;
        };
        for ordinal in 0..meta.index.len() as u32 {
            out.push((id.clone(), ordinal));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::{CommitIndex, PipelineKey, GENESIS_HASH};

    fn commit_segment_envelope(
        key: &PipelineKey,
        index: u64,
        previous: [u8; 32],
        segment_id: &str,
    ) -> crate::buffer::durable::mutation::MutationEnvelope {
        crate::buffer::durable::mutation::MutationEnvelope {
            protocol_version: 1,
            pipeline: key.clone(),
            epoch: skippr_lease::LeaseEpoch::new(1),
            index: CommitIndex::new(index),
            previous_hash: previous,
            payload_sha256: [9u8; 32],
            body: DurableMutation::CommitSegment {
                descriptor: crate::buffer::durable::mutation::SegmentDescriptor {
                    segment_id: segment_id.into(),
                    payload_len: 4,
                    payload_sha256: [9u8; 32],
                    num_partitions: 1,
                    total_bytes: 4,
                    created_at_secs: 0,
                    schema_fingerprints: Vec::new(),
                },
                offsets: Vec::new(),
                checkpoints: Vec::new(),
            },
        }
    }

    fn write_segment(paths: &skippr_lease::PipelinePaths, segment_id: &str, ids: &[&str]) {
        use crate::buffer::segment_file::{PartitionKey, SegmentFile};
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use std::collections::HashMap;
        use std::sync::Arc;

        std::fs::create_dir_all(&paths.segs).unwrap();
        let seg = SegmentFile::new(&paths.segs, segment_id).unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let mut batches = HashMap::new();
        for (i, id) in ids.iter().enumerate() {
            let part = PartitionKey {
                sink_ref: "data_sinks.iceberg".into(),
                namespace: "events".into(),
                partition: format!("p{i}"),
                time: None,
                schema_fingerprint: "fp".into(),
            };
            let batch =
                RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec![*id]))])
                    .unwrap();
            batches.insert(part, vec![batch]);
        }
        seg.write_snapshot(&HashMap::new(), &batches, &HashMap::new(), &HashMap::new())
            .unwrap();
    }

    fn commit(
        log: &mut crate::buffer::durable::log::MutationLog,
        envelope: &crate::buffer::durable::mutation::MutationEnvelope,
    ) {
        log.append_prepared(envelope).unwrap();
        log.append_committed(envelope.index, envelope.entry_hash().unwrap())
            .unwrap();
    }

    #[test]
    fn iceberg_named_segment_is_not_selected() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        write_segment(&paths, "iceberg-seg", &["evt-a"]);
        write_segment(&paths, "live-seg", &["evt-b"]);
        let mut log = crate::buffer::durable::log::MutationLog::open(paths).unwrap();
        let first = commit_segment_envelope(&key, 1, GENESIS_HASH, "iceberg-seg");
        commit(&mut log, &first);
        let second = commit_segment_envelope(&key, 2, first.entry_hash().unwrap(), "live-seg");
        commit(&mut log, &second);
        let selected = select_live_ordinals(&log, &["iceberg-seg".into()]);
        assert_eq!(selected, vec![("live-seg".to_string(), 0)]);
    }

    #[test]
    fn complete_ordinal_stays_visible_until_iceberg_exclude() {
        use crate::buffer::completion_ledger::{SegmentCompletionLedger, SegmentCompletionUpdate};

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        write_segment(&paths, "live-seg", &["evt-0", "evt-1"]);
        let seg = crate::buffer::segment_file::SegmentFile::new(&paths.segs, "live-seg").unwrap();
        let meta = seg.read_metadata().unwrap();
        assert!(meta.index.len() >= 2);
        let ledger = SegmentCompletionLedger::new(paths.completions.clone());
        ledger
            .mark_complete_batch(&[SegmentCompletionUpdate {
                segment_id: "live-seg",
                index: &meta.index,
                ordinals: &[0],
            }])
            .unwrap();
        let mut log = crate::buffer::durable::log::MutationLog::open(paths).unwrap();
        let commit_env = commit_segment_envelope(&key, 1, GENESIS_HASH, "live-seg");
        commit(&mut log, &commit_env);
        let selected = select_live_ordinals(&log, &[]);
        let expected: Vec<(String, u32)> = (0..meta.index.len() as u32)
            .map(|ordinal| ("live-seg".to_string(), ordinal))
            .collect();
        assert_eq!(selected, expected);
        assert!(select_live_ordinals(&log, &["live-seg".into()]).is_empty());
    }

    #[test]
    fn fully_completed_ledger_stays_visible_without_iceberg_exclude() {
        use crate::buffer::completion_ledger::{SegmentCompletionLedger, SegmentCompletionUpdate};

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        write_segment(&paths, "live-seg", &["evt-23"]);
        let seg = crate::buffer::segment_file::SegmentFile::new(&paths.segs, "live-seg").unwrap();
        let meta = seg.read_metadata().unwrap();
        let ledger = SegmentCompletionLedger::new(paths.completions.clone());
        let ordinals: Vec<usize> = (0..meta.index.len()).collect();
        ledger
            .mark_complete_batch(&[SegmentCompletionUpdate {
                segment_id: "live-seg",
                index: &meta.index,
                ordinals: &ordinals,
            }])
            .unwrap();
        let mut log = crate::buffer::durable::log::MutationLog::open(paths).unwrap();
        let commit_env = commit_segment_envelope(&key, 1, GENESIS_HASH, "live-seg");
        commit(&mut log, &commit_env);
        assert_eq!(
            select_live_ordinals(&log, &[]),
            vec![("live-seg".to_string(), 0)]
        );
        assert!(select_live_ordinals(&log, &["live-seg".into()]).is_empty());
    }

    #[test]
    fn missing_segment_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        std::fs::create_dir_all(&paths.segs).unwrap();
        let mut log = crate::buffer::durable::log::MutationLog::open(paths).unwrap();
        let commit_env = commit_segment_envelope(&key, 1, GENESIS_HASH, "gone-seg");
        commit(&mut log, &commit_env);
        assert!(select_live_ordinals(&log, &[]).is_empty());
    }

    #[test]
    fn snapshot_prune_still_lists_uncompacted_segment() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        write_segment(&paths, "live", &["evt-live"]);
        let mut log = crate::buffer::durable::log::MutationLog::open(paths.clone()).unwrap();
        let commit_env = commit_segment_envelope(&key, 1, GENESIS_HASH, "live");
        commit(&mut log, &commit_env);
        crate::buffer::durable::snapshot::retain_live_snapshot(&mut log, &paths, &key).unwrap();
        assert!(log.committed_envelopes().is_empty());
        assert_eq!(
            select_live_ordinals(&log, &[]),
            vec![("live".to_string(), 0)]
        );
    }

    #[test]
    fn reclaimed_segment_is_not_selected() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = skippr_lease::PipelinePaths::new(dir.path(), &key).unwrap();
        write_segment(&paths, "live-seg", &["evt-a"]);
        let mut log = crate::buffer::durable::log::MutationLog::open(paths).unwrap();
        let first = commit_segment_envelope(&key, 1, GENESIS_HASH, "live-seg");
        commit(&mut log, &first);
        let reclaim = crate::buffer::durable::mutation::MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: skippr_lease::LeaseEpoch::new(1),
            index: CommitIndex::new(2),
            previous_hash: first.entry_hash().unwrap(),
            payload_sha256: [0u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "live-seg".into(),
            },
        };
        commit(&mut log, &reclaim);
        assert!(select_live_ordinals(&log, &[]).is_empty());
    }
}
