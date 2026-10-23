use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use skippr_lease::{LeaseEpoch, NodeId, SegmentId};

use crate::error::CatalogError;
use crate::key::CompactionId;

pub const ROW_MAX_BYTES: usize = 350 * 1024;
pub const FORMAT_VERSION: u32 = 3;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct CatalogSeq(u64);

impl CatalogSeq {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub fn next(self) -> Result<Self, CatalogError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| CatalogError::Codec("catalog seq overflow".into()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CatalogFence {
    pub owner: NodeId,
    pub epoch: LeaseEpoch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommittedOffset {
    pub namespace: String,
    pub partition: String,
    pub filesize: u64,
    pub position: u64,
    pub closed: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommittedCheckpoint {
    pub logical_key: String,
    pub envelope: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PayloadLocation {
    Disk { path: String },
    S3 { bucket: String, key: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SliceBitmap {
    slice_count: u32,
    bytes: Vec<u8>,
}

impl SliceBitmap {
    pub fn new(slice_count: u32) -> Self {
        let n = slice_count.div_ceil(8) as usize;
        Self {
            slice_count,
            bytes: vec![0; n],
        }
    }

    pub fn slice_count(&self) -> u32 {
        self.slice_count
    }

    pub fn or_bits(&mut self, ordinals: &[u32]) {
        for &ord in ordinals {
            if ord >= self.slice_count {
                continue;
            }
            let i = ord as usize;
            self.bytes[i / 8] |= 1 << (i % 8);
        }
    }

    pub fn is_done(&self, ordinal: u32) -> bool {
        if ordinal >= self.slice_count {
            return false;
        }
        let i = ordinal as usize;
        self.bytes[i / 8] & (1 << (i % 8)) != 0
    }

    pub fn all_done(&self) -> bool {
        if self.slice_count == 0 {
            return true;
        }
        for i in 0..self.slice_count as usize {
            if self.bytes[i / 8] & (1 << (i % 8)) == 0 {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SliceIndexEntry {
    pub segment_id: SegmentId,
    pub ordinal: u32,
    pub namespace: String,
    pub partition: String,
    pub start: u64,
    pub len: u64,
    pub codec: u8,
    pub version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SliceDirEntry {
    pub ordinal: u32,
    pub namespace: String,
    pub partition: String,
    pub event_time: Option<i64>,
    pub start: u64,
    pub len: u64,
    pub codec: u8,
    pub row_count: Option<u64>,
    pub sink_ref: String,
    pub schema_fingerprint: String,
    pub cdc_meta_hash: Option<[u8; 32]>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SegmentIndexRow {
    pub segment_id: SegmentId,
    pub version: u64,
    pub entries: Vec<SliceDirEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SegmentRow {
    pub segment_id: SegmentId,
    pub seq: CatalogSeq,
    pub location: PayloadLocation,
    pub payload_len: u64,
    pub payload_sha256: [u8; 32],
    pub slice_count: u32,
    pub done: SliceBitmap,
    pub offsets: Vec<CommittedOffset>,
    pub checkpoints: Vec<CommittedCheckpoint>,
    pub epoch: LeaseEpoch,
    pub version: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CompactionState {
    Pending,
    Sent,
    Acked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactionRow {
    pub id: CompactionId,
    pub state: CompactionState,
    pub body: Vec<u8>,
    pub version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ControlBody {
    Checkpoint {
        logical_key: String,
        envelope: Vec<u8>,
    },
    OffsetHint {
        position: u64,
        closed: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlRow {
    pub seq: CatalogSeq,
    pub epoch: LeaseEpoch,
    pub body: ControlBody,
    pub version: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SeqRow {
    pub max_seq: CatalogSeq,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FormatPhase {
    Imported,
    Clean,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FormatRow {
    pub version: u32,
    pub phase: FormatPhase,
    pub row_version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CatalogRow {
    Segment(SegmentRow),
    Index(SegmentIndexRow),
    Slice(SliceIndexEntry),
    Compaction(CompactionRow),
    Control(ControlRow),
    Seq(SeqRow),
    Format(FormatRow),
}

impl CatalogRow {
    pub fn encode(&self) -> Result<Vec<u8>, CatalogError> {
        let bytes = bincode::serialize(self).map_err(|err| CatalogError::Codec(err.to_string()))?;
        if bytes.len() > ROW_MAX_BYTES {
            return Err(CatalogError::RowTooLarge { bytes: bytes.len() });
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CatalogError> {
        if bytes.len() > ROW_MAX_BYTES {
            return Err(CatalogError::RowTooLarge { bytes: bytes.len() });
        }
        bincode::deserialize(bytes).map_err(|err| CatalogError::Codec(err.to_string()))
    }

    pub fn sha256(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PutCond {
    NotExists,
    SameSha,
    Version(u64),
    SeqAdvance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteCond {
    Exists,
    Version(u64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogOp {
    Put {
        key: crate::key::CatalogKey,
        row: CatalogRow,
        cond: PutCond,
    },
    Delete {
        key: crate::key::CatalogKey,
        cond: DeleteCond,
    },
}

impl CatalogOp {
    pub fn encoded_len(&self) -> Result<usize, CatalogError> {
        match self {
            Self::Put { row, .. } => Ok(row.encode()?.len()),
            Self::Delete { .. } => Ok(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::SegmentId;

    #[test]
    fn slice_bitmap_or_is_idempotent_and_all_done() {
        let mut bits = SliceBitmap::new(3);
        assert!(!bits.all_done());
        bits.or_bits(&[0, 2]);
        bits.or_bits(&[0, 2]);
        assert!(!bits.all_done());
        bits.or_bits(&[1]);
        assert!(bits.all_done());
    }

    #[test]
    fn segment_row_roundtrip_under_ceiling() {
        let row = CatalogRow::Segment(SegmentRow {
            segment_id: SegmentId::new("s").unwrap(),
            seq: CatalogSeq::new(1),
            location: PayloadLocation::Disk {
                path: "/tmp/s.seg".into(),
            },
            payload_len: 8,
            payload_sha256: [0; 32],
            slice_count: 1,
            done: SliceBitmap::new(1),
            offsets: vec![CommittedOffset {
                namespace: "ns".into(),
                partition: "p".into(),
                filesize: 0,
                position: 1,
                closed: 1,
            }],
            checkpoints: vec![],
            epoch: LeaseEpoch::new(1),
            version: 0,
        });
        let bytes = row.encode().unwrap();
        assert!(bytes.len() < ROW_MAX_BYTES);
        assert_eq!(CatalogRow::decode(&bytes).unwrap(), row);
    }

    #[test]
    fn slice_index_entry_is_the_query_authority() {
        let row = CatalogRow::Slice(SliceIndexEntry {
            segment_id: SegmentId::new("s").unwrap(),
            ordinal: 0,
            namespace: "pipe".into(),
            partition: "p".into(),
            start: 16,
            len: 32,
            codec: 0,
            version: 0,
        });
        let bytes = row.encode().unwrap();
        assert_eq!(CatalogRow::decode(&bytes).unwrap(), row);
    }

    #[test]
    fn segment_index_row_is_the_query_authority() {
        assert_eq!(FORMAT_VERSION, 3);
        let row = CatalogRow::Index(SegmentIndexRow {
            segment_id: SegmentId::new("s").unwrap(),
            version: 0,
            entries: vec![SliceDirEntry {
                ordinal: 0,
                namespace: "pipe".into(),
                partition: "p".into(),
                event_time: Some(1),
                start: 16,
                len: 32,
                codec: 0,
                row_count: None,
                sink_ref: "out".into(),
                schema_fingerprint: "fp".into(),
                cdc_meta_hash: None,
            }],
        });
        let bytes = row.encode().unwrap();
        assert!(bytes.len() < ROW_MAX_BYTES);
        assert_eq!(CatalogRow::decode(&bytes).unwrap(), row);
    }

    #[test]
    fn oversize_compaction_body_is_rejected() {
        let row = CatalogRow::Compaction(CompactionRow {
            id: CompactionId::new("c").unwrap(),
            state: CompactionState::Sent,
            body: vec![0; ROW_MAX_BYTES],
            version: 0,
        });
        assert!(matches!(
            row.encode(),
            Err(CatalogError::RowTooLarge { .. })
        ));
    }
}
