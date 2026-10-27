//! Iceberg snapshot exclude-set identity: `(segment_id, start)`.
//!
//! `skippr.wal-slices` is the only property. Encode is comma-joined `seg@start`.
//! `SegmentId` already rejects `,` and `@`, so a typed [`WalSliceRef`] cannot
//! produce an ambiguous encoding.

use std::collections::{BTreeSet, HashSet};
use std::fmt;

use serde_derive::{Deserialize, Serialize};
use skippr_lease::SegmentId;

pub const SNAPSHOT_WAL_SLICES: &str = "skippr.wal-slices";
pub const ANCESTRY_WALK_CEILING: usize = 10_000;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct WalSliceRef {
    pub segment_id: SegmentId,
    pub start: u64,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum WalSliceRefError {
    #[error("malformed wal slice ref {0:?}")]
    Malformed(String),
    #[error("segment id {0:?} is not a valid path component")]
    InvalidSegment(String),
    #[error("Iceberg snapshot ancestry exceeds walk ceiling")]
    AncestryTooLong,
}

impl WalSliceRef {
    pub fn new(segment_id: SegmentId, start: u64) -> Self {
        Self { segment_id, start }
    }

    pub fn try_from_parts(segment_id: &str, start: u64) -> Result<Self, WalSliceRefError> {
        let segment_id = SegmentId::new(segment_id)
            .map_err(|_| WalSliceRefError::InvalidSegment(segment_id.to_string()))?;
        Ok(Self { segment_id, start })
    }
}

impl fmt::Display for WalSliceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.segment_id.as_str(), self.start)
    }
}

pub fn encode_wal_slices<'a>(refs: impl IntoIterator<Item = &'a WalSliceRef>) -> String {
    let ordered: BTreeSet<_> = refs.into_iter().collect();
    ordered
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

pub fn decode_wal_slices(value: &str) -> Result<BTreeSet<WalSliceRef>, WalSliceRefError> {
    if value.is_empty() {
        return Ok(BTreeSet::new());
    }
    value
        .split(',')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (seg, start) = part
                .rsplit_once('@')
                .ok_or_else(|| WalSliceRefError::Malformed(part.to_string()))?;
            if seg.is_empty() {
                return Err(WalSliceRefError::Malformed(part.to_string()));
            }
            let start = start
                .parse::<u64>()
                .map_err(|_| WalSliceRefError::Malformed(part.to_string()))?;
            WalSliceRef::try_from_parts(seg, start)
        })
        .collect()
}

/// Walk Iceberg ancestry of `start`, decoding `skippr.wal-slices` on each
/// snapshot. A missing snapshot ends the walk (expired parent). A malformed
/// property fails the whole set.
pub fn walk_ancestry_wal_slices(
    start: i64,
    mut lookup: impl FnMut(i64) -> Option<(Option<i64>, Option<String>)>,
) -> Result<HashSet<WalSliceRef>, WalSliceRefError> {
    let mut out = HashSet::new();
    let mut next = Some(start);
    for _ in 0..ANCESTRY_WALK_CEILING {
        let Some(id) = next else {
            return Ok(out);
        };
        let Some((parent, prop)) = lookup(id) else {
            return Ok(out);
        };
        if let Some(value) = prop {
            out.extend(decode_wal_slices(&value)?);
        }
        next = parent;
    }
    Err(WalSliceRefError::AncestryTooLong)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn r(seg: &str, start: u64) -> WalSliceRef {
        WalSliceRef::try_from_parts(seg, start).unwrap()
    }

    #[test]
    fn encode_decode_round_trip() {
        let refs = BTreeSet::from([r("seg-a", 0), r("seg-a", 100), r("seg-b", 0)]);
        let encoded = encode_wal_slices(&refs);
        assert_eq!(encoded, "seg-a@0,seg-a@100,seg-b@0");
        assert_eq!(decode_wal_slices(&encoded).unwrap(), refs);
    }

    #[test]
    fn empty_is_empty_set() {
        assert!(decode_wal_slices("").unwrap().is_empty());
        assert_eq!(encode_wal_slices(&BTreeSet::new()), "");
    }

    #[test]
    fn missing_at_is_malformed() {
        assert!(matches!(
            decode_wal_slices("seg-a"),
            Err(WalSliceRefError::Malformed(_))
        ));
    }

    #[test]
    fn reserved_at_in_segment_is_invalid() {
        assert!(matches!(
            decode_wal_slices("seg@a@0"),
            Err(WalSliceRefError::InvalidSegment(_))
        ));
    }

    #[test]
    fn hash_in_segment_is_invalid() {
        assert!(matches!(
            decode_wal_slices("seg#a@0"),
            Err(WalSliceRefError::InvalidSegment(_))
        ));
    }

    #[test]
    fn ancestry_collects_parent_slices() {
        let out = walk_ancestry_wal_slices(2, |id| match id {
            2 => Some((Some(1), Some("child@10".into()))),
            1 => Some((None, Some("parent@0".into()))),
            _ => None,
        })
        .unwrap();
        assert_eq!(out, HashSet::from([r("child", 10), r("parent", 0)]));
    }

    #[test]
    fn ancestry_malformed_property_is_error() {
        let err = walk_ancestry_wal_slices(1, |_| Some((None, Some("seg-a".into())))).unwrap_err();
        assert!(matches!(err, WalSliceRefError::Malformed(_)));
    }

    #[test]
    fn ancestry_stops_at_missing_parent() {
        let out = walk_ancestry_wal_slices(1, |id| match id {
            1 => Some((Some(99), Some("live@0".into()))),
            _ => None,
        })
        .unwrap();
        assert_eq!(out, HashSet::from([r("live", 0)]));
    }

    #[test]
    fn ancestry_ignores_sibling_snapshots() {
        let out = walk_ancestry_wal_slices(2, |id| match id {
            2 => Some((Some(1), Some("child@0".into()))),
            1 => Some((None, Some("parent@0".into()))),
            3 => Some((None, Some("sibling@0".into()))),
            _ => None,
        })
        .unwrap();
        assert_eq!(out, HashSet::from([r("child", 0), r("parent", 0)]));
    }

    proptest! {
        #[test]
        fn encode_decode_identity(starts in proptest::collection::btree_set(0u64..10_000, 0..8)) {
            let refs: BTreeSet<WalSliceRef> = starts
                .into_iter()
                .map(|start| r("seg-x", start))
                .collect();
            let encoded = encode_wal_slices(&refs);
            prop_assert_eq!(decode_wal_slices(&encoded).unwrap(), refs);
        }
    }
}
