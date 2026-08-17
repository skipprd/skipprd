use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use skippr_lease::{
    CommitIndex, DurableError, LeaseEpoch, PipelineKey, GENESIS_HASH, PROTOCOL_MAX,
};

use crate::buffer::compaction_transaction::CompactionTransaction;
use crate::helpers::offsets::{OffsetKey, OffsetValue};
use crate::plugins::cdc::CheckpointEnvelope;

pub const CLUSTER_FORMAT_V1: &str = "CLUSTER_FORMAT_V1";

#[derive(Clone, Debug)]
pub struct MutationEnvelope {
    pub protocol_version: u32,
    pub pipeline: PipelineKey,
    pub epoch: LeaseEpoch,
    pub index: CommitIndex,
    pub previous_hash: [u8; 32],
    pub body: DurableMutation,
    pub payload_sha256: [u8; 32],
}

impl MutationEnvelope {
    pub fn entry_hash(&self) -> Result<[u8; 32], DurableError> {
        let proto_bytes = super::codec::encode_envelope_proto(self)?;
        let mut hasher = Sha256::new();
        hasher.update(self.previous_hash);
        hasher.update(&proto_bytes);
        hasher.update(self.payload_sha256);
        Ok(hasher.finalize().into())
    }
}

#[derive(Clone, Debug)]
pub enum DurableMutation {
    CommitSegment {
        descriptor: SegmentDescriptor,
        offsets: Vec<CommittedOffset>,
        checkpoints: Vec<CommittedCheckpoint>,
    },
    PutCompaction {
        transaction: CompactionTransaction,
    },
    CompleteSlices {
        compaction_id: String,
        entries: Vec<CompletedOrdinals>,
    },
    ReclaimSegment {
        segment_id: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SegmentDescriptor {
    pub segment_id: String,
    pub payload_len: u64,
    pub payload_sha256: [u8; 32],
    pub num_partitions: u32,
    pub total_bytes: u64,
    pub created_at_secs: u64,
    pub schema_fingerprints: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommittedOffset {
    pub namespace: String,
    pub partition: String,
    pub filesize: u64,
    pub position: u64,
    pub closed: u64,
}

impl CommittedOffset {
    pub fn from_key_value(key: &OffsetKey, value: &OffsetValue) -> Self {
        Self {
            namespace: key.namespace.clone(),
            partition: key.partition.clone(),
            filesize: value.filesize.get(),
            position: value.line.get(),
            closed: value.closed.get(),
        }
    }

    /// Offset tuple published with a WAL `CommitSegment`.
    ///
    /// Immutable sources treat the durable commit as end-of-object, so `closed`
    /// is always `1`. Clustered mode publishes this tuple to DynamoDB and never
    /// follows up with `mark_offsets_durable_in_wal`.
    pub fn for_wal_commit(key: &OffsetKey, position: u64, existing: Option<&OffsetValue>) -> Self {
        Self {
            namespace: key.namespace.clone(),
            partition: key.partition.clone(),
            filesize: existing.map(|value| value.filesize.get()).unwrap_or(0),
            position,
            closed: 1,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommittedCheckpoint {
    pub logical_key: String,
    pub envelope: Vec<u8>,
}

impl CommittedCheckpoint {
    pub fn from_envelope(key: &str, envelope: &CheckpointEnvelope) -> Result<Self, String> {
        Ok(Self {
            logical_key: key.to_string(),
            envelope: bincode::serialize(envelope).map_err(|err| err.to_string())?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct CompletedOrdinals {
    pub segment_id: String,
    pub ordinals: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryComparison {
    Next,
    AlreadyAppliedSameHash,
    SameIndexDifferentHash,
    Gap { head: CommitIndex },
}

pub fn genesis_hash() -> [u8; 32] {
    GENESIS_HASH
}

pub fn protocol_ok(version: u32) -> bool {
    version == PROTOCOL_MAX || version + 1 == PROTOCOL_MAX
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::PipelineKey;

    #[test]
    fn hash_chain_is_deterministic() {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let env = MutationEnvelope {
            protocol_version: 1,
            pipeline: key,
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [1u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "seg-1".into(),
            },
        };
        let first = env.entry_hash().unwrap();
        let second = env.entry_hash().unwrap();
        assert_eq!(first, second);
        assert_ne!(first, GENESIS_HASH);
    }

    #[test]
    fn wal_commit_offset_marks_partition_closed() {
        let key = OffsetKey::new("events", "/tmp/batch1.jsonl");
        let committed = CommittedOffset::for_wal_commit(&key, 5, None);
        assert_eq!(committed.namespace, "events");
        assert_eq!(committed.partition, "/tmp/batch1.jsonl");
        assert_eq!(committed.position, 5);
        assert_eq!(committed.filesize, 0);
        assert_eq!(committed.closed, 1);
    }

    #[test]
    fn wal_commit_offset_keeps_existing_filesize() {
        let key = OffsetKey::new("events", "part");
        let existing = OffsetValue {
            filesize: zerocopy::U64::new(99),
            line: zerocopy::U64::new(3),
            closed: zerocopy::U64::new(0),
        };
        let committed = CommittedOffset::for_wal_commit(&key, 7, Some(&existing));
        assert_eq!(committed.filesize, 99);
        assert_eq!(committed.position, 7);
        assert_eq!(committed.closed, 1);
    }
}
