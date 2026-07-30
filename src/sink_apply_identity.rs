use crate::buffer::compaction_transaction::SinkWriteSemantics;
use crate::plugins::source_contract::{SourceNamespaceContract, WritePolicy};
use crate::runtime_plugins::protocol::RuntimeWalPartRef;
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;

pub const SINK_APPLY_ENVELOPE_V2: u32 = 2;

/// The fields that make one WAL part distinct for apply/replay purposes.
pub trait WalRefIdentity {
    fn segment_id(&self) -> &str;
    fn source(&self) -> Cow<'_, str>;
    fn start(&self) -> u64;
    fn len(&self) -> u64;
    fn sink_ref(&self) -> &str;
    fn namespace(&self) -> &str;
    fn partition(&self) -> &str;
    fn time(&self) -> Option<i64>;
    fn schema_fingerprint(&self) -> &str;
    fn cdc_meta_hash(&self) -> Option<&[u8; 32]>;
}

impl WalRefIdentity for RuntimeWalPartRef {
    fn segment_id(&self) -> &str {
        &self.segment_id
    }

    fn source(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.source)
    }

    fn start(&self) -> u64 {
        self.start
    }

    fn len(&self) -> u64 {
        self.len
    }

    fn sink_ref(&self) -> &str {
        &self.sink_ref
    }

    fn namespace(&self) -> &str {
        &self.namespace
    }

    fn partition(&self) -> &str {
        &self.partition
    }

    fn time(&self) -> Option<i64> {
        self.time
    }

    fn schema_fingerprint(&self) -> &str {
        &self.schema_fingerprint
    }

    fn cdc_meta_hash(&self) -> Option<&[u8; 32]> {
        self.cdc_meta_hash.as_ref()
    }
}

/// Stable bytes shared by compaction IDs and v2 WAL-ref fingerprints.
///
/// Keep this layout byte-for-byte compatible with the original compaction ID
/// implementation. In particular, non-CDC compaction IDs must not change.
pub fn wal_ref_identity_bytes(wal_ref: &(impl WalRefIdentity + ?Sized)) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(wal_ref.segment_id().as_bytes());
    out.push(0);
    out.extend_from_slice(wal_ref.source().as_bytes());
    out.push(0);
    out.extend_from_slice(&wal_ref.start().to_le_bytes());
    out.extend_from_slice(&wal_ref.len().to_le_bytes());
    out.extend_from_slice(wal_ref.sink_ref().as_bytes());
    out.push(0);
    out.extend_from_slice(wal_ref.namespace().as_bytes());
    out.push(0);
    out.extend_from_slice(wal_ref.partition().as_bytes());
    out.push(0);
    out.extend_from_slice(&wal_ref.time().unwrap_or(0).to_le_bytes());
    out.extend_from_slice(wal_ref.schema_fingerprint().as_bytes());
    if let Some(hash) = wal_ref.cdc_meta_hash() {
        out.extend_from_slice(hash);
    }
    out
}

pub fn canonical_sorted_wal_ref_identity_bytes<T: WalRefIdentity>(wal_refs: &[T]) -> Vec<Vec<u8>> {
    let mut identities = wal_refs
        .iter()
        .map(wal_ref_identity_bytes)
        .collect::<Vec<_>>();
    identities.sort();
    identities
}

/// Canonical v2 hash for a set of WAL refs.
pub fn canonical_wal_refs_fingerprint<T: WalRefIdentity>(wal_refs: &[T]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"grouped-wal-refs-v2");
    for identity in canonical_sorted_wal_ref_identity_bytes(wal_refs) {
        hasher.update(identity);
        hasher.update([0xff]);
    }
    hex::encode(hasher.finalize())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CanonicalWalPartRef {
    pub segment_id: String,
    pub source: String,
    pub start: u64,
    pub len: u64,
    pub sink_ref: String,
    pub namespace: String,
    pub partition: String,
    pub time: Option<i64>,
    pub schema_fingerprint: String,
    pub cdc_meta_hash: Option<[u8; 32]>,
}

impl From<&RuntimeWalPartRef> for CanonicalWalPartRef {
    fn from(wal_ref: &RuntimeWalPartRef) -> Self {
        Self {
            segment_id: wal_ref.segment_id.clone(),
            source: wal_ref.source.clone(),
            start: wal_ref.start,
            len: wal_ref.len,
            sink_ref: wal_ref.sink_ref.clone(),
            namespace: wal_ref.namespace.clone(),
            partition: wal_ref.partition.clone(),
            time: wal_ref.time,
            schema_fingerprint: wal_ref.schema_fingerprint.clone(),
            cdc_meta_hash: wal_ref.cdc_meta_hash,
        }
    }
}

impl WalRefIdentity for CanonicalWalPartRef {
    fn segment_id(&self) -> &str {
        &self.segment_id
    }

    fn source(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.source)
    }

    fn start(&self) -> u64 {
        self.start
    }

    fn len(&self) -> u64 {
        self.len
    }

    fn sink_ref(&self) -> &str {
        &self.sink_ref
    }

    fn namespace(&self) -> &str {
        &self.namespace
    }

    fn partition(&self) -> &str {
        &self.partition
    }

    fn time(&self) -> Option<i64> {
        self.time
    }

    fn schema_fingerprint(&self) -> &str {
        &self.schema_fingerprint
    }

    fn cdc_meta_hash(&self) -> Option<&[u8; 32]> {
        self.cdc_meta_hash.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkApplyGroupKind {
    Append,
    Cdc,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SinkApplyGroupingIdentity {
    pub sink_ref: String,
    pub namespace: String,
    pub partition: String,
    pub time: Option<i64>,
    pub kind: SinkApplyGroupKind,
}

impl SinkApplyGroupingIdentity {
    pub fn from_wal_refs(wal_refs: &[RuntimeWalPartRef], is_cdc: bool) -> Self {
        let first = wal_refs.first();
        Self {
            sink_ref: first
                .map(|wal_ref| wal_ref.sink_ref.clone())
                .unwrap_or_default(),
            namespace: first
                .map(|wal_ref| wal_ref.namespace.clone())
                .unwrap_or_default(),
            partition: first
                .map(|wal_ref| wal_ref.partition.clone())
                .unwrap_or_default(),
            time: first.and_then(|wal_ref| wal_ref.time),
            kind: if is_cdc {
                SinkApplyGroupKind::Cdc
            } else {
                SinkApplyGroupKind::Append
            },
        }
    }
}

/// Canonical apply identity introduced ahead of runtime protocol v17.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SinkApplyEnvelopeV2 {
    pub version: u32,
    pub compaction_id: String,
    pub idempotency_key: String,
    pub wal_refs: Vec<CanonicalWalPartRef>,
    pub wal_refs_fingerprint: String,
    pub grouping: SinkApplyGroupingIdentity,
    pub write_policy: WritePolicy,
    pub write_semantics: SinkWriteSemantics,
    pub schema_fingerprint: Option<String>,
    pub schema_version: Option<u64>,
    pub source_contract: Option<SourceNamespaceContract>,
}

impl SinkApplyEnvelopeV2 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        compaction_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        wal_refs: &[RuntimeWalPartRef],
        is_cdc: bool,
        write_policy: WritePolicy,
        write_semantics: SinkWriteSemantics,
        schema_fingerprint: Option<String>,
        schema_version: Option<u64>,
        source_contract: Option<SourceNamespaceContract>,
    ) -> Self {
        let wal_refs_fingerprint = canonical_wal_refs_fingerprint(wal_refs);
        let grouping = SinkApplyGroupingIdentity::from_wal_refs(wal_refs, is_cdc);
        let mut wal_refs = wal_refs
            .iter()
            .map(CanonicalWalPartRef::from)
            .collect::<Vec<_>>();
        wal_refs.sort_by_key(wal_ref_identity_bytes);
        Self {
            version: SINK_APPLY_ENVELOPE_V2,
            compaction_id: compaction_id.into(),
            idempotency_key: idempotency_key.into(),
            wal_refs,
            wal_refs_fingerprint,
            grouping,
            write_policy,
            write_semantics,
            schema_fingerprint,
            schema_version,
            source_contract,
        }
    }

    pub fn has_cdc_metadata(&self) -> bool {
        self.wal_refs
            .iter()
            .any(|wal_ref| wal_ref.cdc_meta_hash.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal_ref(segment_id: &str, cdc_meta_hash: Option<[u8; 32]>) -> RuntimeWalPartRef {
        RuntimeWalPartRef {
            segment_id: segment_id.to_string(),
            source: format!("s3://bucket/{segment_id}"),
            start: 1,
            len: 10,
            sink_ref: "sink.main".to_string(),
            namespace: "ns".to_string(),
            partition: "p=1".to_string(),
            time: Some(1),
            schema_fingerprint: "schema".to_string(),
            cdc_meta_hash,
        }
    }

    #[test]
    fn canonical_fingerprint_is_ref_order_independent() {
        let refs_a = vec![wal_ref("b", None), wal_ref("a", None)];
        let refs_b = vec![wal_ref("a", None), wal_ref("b", None)];
        assert_eq!(
            canonical_wal_refs_fingerprint(&refs_a),
            canonical_wal_refs_fingerprint(&refs_b)
        );
    }

    #[test]
    fn canonical_fingerprint_includes_cdc_metadata_hash() {
        let refs_a = vec![wal_ref("a", Some([1; 32]))];
        let refs_b = vec![wal_ref("a", Some([2; 32]))];
        assert_ne!(
            canonical_wal_refs_fingerprint(&refs_a),
            canonical_wal_refs_fingerprint(&refs_b)
        );
    }

    #[test]
    fn envelope_v2_is_canonical_and_serializable() {
        let refs = vec![wal_ref("b", Some([1; 32])), wal_ref("a", Some([2; 32]))];
        let envelope = SinkApplyEnvelopeV2::new(
            "c1",
            "k1",
            &refs,
            true,
            WritePolicy::MergeByKey,
            SinkWriteSemantics::ExactOnce,
            Some("schema".to_string()),
            Some(7),
            None,
        );
        assert_eq!(envelope.version, SINK_APPLY_ENVELOPE_V2);
        assert_eq!(envelope.wal_refs[0].segment_id, "a");
        assert_eq!(envelope.grouping.kind, SinkApplyGroupKind::Cdc);
        assert_eq!(envelope.schema_version, Some(7));

        let bytes = serde_json::to_vec(&envelope).unwrap();
        let decoded: SinkApplyEnvelopeV2 = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, envelope);
    }
}
