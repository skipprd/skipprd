use skippr_lease::{validate_path_component, PathError, SegmentId};

#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
pub struct CompactionId(String);

impl CompactionId {
    pub fn new(raw: impl Into<String>) -> Result<Self, PathError> {
        let value = raw.into();
        validate_path_component(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(
    Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, serde::Deserialize, serde::Serialize,
)]
pub struct CheckpointKey(String);

impl CheckpointKey {
    pub fn new(raw: impl Into<String>) -> Result<Self, PathError> {
        let value = raw.into();
        validate_path_component(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OffsetHintKey {
    namespace: String,
    partition: String,
}

impl OffsetHintKey {
    pub fn new(
        namespace: impl Into<String>,
        partition: impl Into<String>,
    ) -> Result<Self, PathError> {
        let namespace = namespace.into();
        let partition = partition.into();
        validate_path_component(&namespace)?;
        validate_path_component(&partition)?;
        Ok(Self {
            namespace,
            partition,
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn partition(&self) -> &str {
        &self.partition
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CatalogPrefix {
    Segment,
    Index,
    Slice,
    Compaction,
    Checkpoint,
    OffsetHint,
}

impl CatalogPrefix {
    pub const ALL: [Self; 6] = [
        Self::Segment,
        Self::Index,
        Self::Slice,
        Self::Compaction,
        Self::Checkpoint,
        Self::OffsetHint,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Segment => "wal#seg#",
            Self::Index => "wal#idx#",
            Self::Slice => "wal#slice#",
            Self::Compaction => "wal#cmp#",
            Self::Checkpoint => "wal#cp#",
            Self::OffsetHint => "wal#hint#",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CatalogKey {
    Segment(SegmentId),
    Index(SegmentId),
    Slice { segment_id: SegmentId, ordinal: u32 },
    Compaction(CompactionId),
    Checkpoint(CheckpointKey),
    OffsetHint(OffsetHintKey),
    Seq,
    Format,
}

impl CatalogKey {
    pub fn encode(&self) -> String {
        match self {
            Self::Segment(id) => format!("wal#seg#{}", id.as_str()),
            Self::Index(id) => format!("wal#idx#{}", id.as_str()),
            Self::Slice {
                segment_id,
                ordinal,
            } => format!("wal#slice#{}#{ordinal}", segment_id.as_str()),
            Self::Compaction(id) => format!("wal#cmp#{}", id.as_str()),
            Self::Checkpoint(key) => format!("wal#cp#{}", key.as_str()),
            Self::OffsetHint(key) => {
                format!("wal#hint#{}#{}", key.namespace(), key.partition())
            }
            Self::Seq => "wal#seq".to_string(),
            Self::Format => "wal#format".to_string(),
        }
    }

    pub fn decode(sk: &str) -> Result<Self, PathError> {
        if sk == "wal#seq" {
            return Ok(Self::Seq);
        }
        if sk == "wal#format" {
            return Ok(Self::Format);
        }
        if let Some(rest) = sk.strip_prefix("wal#seg#") {
            return Ok(Self::Segment(SegmentId::new(rest)?));
        }
        if let Some(rest) = sk.strip_prefix("wal#idx#") {
            return Ok(Self::Index(SegmentId::new(rest)?));
        }
        if let Some(rest) = sk.strip_prefix("wal#slice#") {
            let (segment, ordinal) = rest.rsplit_once('#').ok_or_else(|| {
                PathError::InvalidComponent(format!("slice SK must be wal#slice#seg#ordinal: {sk}"))
            })?;
            let ordinal: u32 = ordinal.parse().map_err(|_| {
                PathError::InvalidComponent(format!("slice ordinal is not u32: {sk}"))
            })?;
            return Ok(Self::Slice {
                segment_id: SegmentId::new(segment)?,
                ordinal,
            });
        }
        if let Some(rest) = sk.strip_prefix("wal#cmp#") {
            return Ok(Self::Compaction(CompactionId::new(rest)?));
        }
        if let Some(rest) = sk.strip_prefix("wal#cp#") {
            return Ok(Self::Checkpoint(CheckpointKey::new(rest)?));
        }
        if let Some(rest) = sk.strip_prefix("wal#hint#") {
            let (namespace, partition) = rest.split_once('#').ok_or_else(|| {
                PathError::InvalidComponent(format!(
                    "offset hint SK must be wal#hint#ns#part: {sk}"
                ))
            })?;
            return Ok(Self::OffsetHint(OffsetHintKey::new(namespace, partition)?));
        }
        Err(PathError::InvalidComponent(format!(
            "unknown catalog SK: {sk}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::SegmentId;

    #[test]
    fn codec_roundtrips_every_variant() {
        let keys = [
            CatalogKey::Segment(SegmentId::new("seg-1").unwrap()),
            CatalogKey::Index(SegmentId::new("seg-1").unwrap()),
            CatalogKey::Slice {
                segment_id: SegmentId::new("seg-1").unwrap(),
                ordinal: 3,
            },
            CatalogKey::Compaction(CompactionId::new("cmp-1").unwrap()),
            CatalogKey::Checkpoint(CheckpointKey::new("cp-1").unwrap()),
            CatalogKey::OffsetHint(OffsetHintKey::new("ns", "part").unwrap()),
            CatalogKey::Seq,
            CatalogKey::Format,
        ];
        for key in keys {
            let encoded = key.encode();
            assert_eq!(CatalogKey::decode(&encoded).unwrap(), key);
            assert!(
                !encoded.contains('?'),
                "SK must be produced only by CatalogKey::encode"
            );
        }
    }

    #[test]
    fn decode_rejects_unknown_and_hash_in_component() {
        assert!(CatalogKey::decode("wal#other").is_err());
        assert!(CatalogKey::decode("wal#seg#a#b").is_err());
        assert!(OffsetHintKey::new("ns#x", "p").is_err());
    }

    #[test]
    fn prefixes_are_stable() {
        assert_eq!(CatalogPrefix::Segment.as_str(), "wal#seg#");
        assert_eq!(CatalogPrefix::Index.as_str(), "wal#idx#");
        assert_eq!(CatalogPrefix::Slice.as_str(), "wal#slice#");
        assert_eq!(CatalogPrefix::Compaction.as_str(), "wal#cmp#");
        assert_eq!(CatalogPrefix::Checkpoint.as_str(), "wal#cp#");
        assert_eq!(CatalogPrefix::OffsetHint.as_str(), "wal#hint#");
    }

    #[test]
    fn slice_key_is_segment_plus_ordinal() {
        let key = CatalogKey::Slice {
            segment_id: SegmentId::new("live-seg").unwrap(),
            ordinal: 0,
        };
        assert_eq!(key.encode(), "wal#slice#live-seg#0");
        assert_eq!(CatalogKey::decode("wal#slice#live-seg#0").unwrap(), key);
    }

    #[test]
    fn index_key_is_segment_id() {
        let key = CatalogKey::Index(SegmentId::new("live-seg").unwrap());
        assert_eq!(key.encode(), "wal#idx#live-seg");
        assert_eq!(CatalogKey::decode("wal#idx#live-seg").unwrap(), key);
    }
}
