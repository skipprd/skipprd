use crate::buffer::segment_file::PartitionKey;
use crate::plugins::source_contract::WritePolicy;
use crate::runtime_plugins::protocol::RuntimeWalPartRef;
use crate::sink_apply_identity::{
    canonical_sorted_wal_ref_identity_bytes, wal_ref_identity_bytes, WalRefIdentity,
};
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SegmentSourceDescriptor {
    Disk { path: PathBuf },
    S3 { bucket: String, key: String },
    Wal { uri: String },
}

impl SegmentSourceDescriptor {
    pub fn stable_id(&self) -> String {
        match self {
            Self::Disk { path } => path.to_string_lossy().to_string(),
            Self::S3 { bucket, key } => format!("s3://{bucket}/{key}"),
            Self::Wal { uri } => uri.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum SinkRetrySemantics {
    TransactionalIdempotent,
    DeterministicOverwrite,
    FinalStateIdempotent,
    AtLeastOnce,
    #[default]
    NonRetryable,
}

impl SinkRetrySemantics {
    pub const fn equals(self, other: Self) -> bool {
        match (self, other) {
            (Self::TransactionalIdempotent, Self::TransactionalIdempotent) => true,
            (Self::DeterministicOverwrite, Self::DeterministicOverwrite) => true,
            (Self::FinalStateIdempotent, Self::FinalStateIdempotent) => true,
            (Self::AtLeastOnce, Self::AtLeastOnce) => true,
            (Self::NonRetryable, Self::NonRetryable) => true,
            _ => false,
        }
    }

    pub const fn requires_idempotent_replay(self) -> bool {
        match self {
            Self::TransactionalIdempotent
            | Self::DeterministicOverwrite
            | Self::FinalStateIdempotent => true,
            Self::AtLeastOnce | Self::NonRetryable => false,
        }
    }

    /// Host grouped compaction maps these retries to `SinkWriteSemantics::ExactOnce`.
    pub const fn grouped_writes_are_exact_once(self) -> bool {
        match self {
            Self::TransactionalIdempotent | Self::FinalStateIdempotent => true,
            Self::DeterministicOverwrite | Self::AtLeastOnce | Self::NonRetryable => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum SinkGroupingSupport {
    #[default]
    None,
    AppendOnlyBatches,
    CdcEncodedBatches,
    FinalStateBatches,
}

impl SinkGroupingSupport {
    pub const fn is_none(self) -> bool {
        match self {
            Self::None => true,
            Self::AppendOnlyBatches | Self::CdcEncodedBatches | Self::FinalStateBatches => false,
        }
    }

    pub const fn equals(self, other: Self) -> bool {
        match (self, other) {
            (Self::None, Self::None) => true,
            (Self::AppendOnlyBatches, Self::AppendOnlyBatches) => true,
            (Self::CdcEncodedBatches, Self::CdcEncodedBatches) => true,
            (Self::FinalStateBatches, Self::FinalStateBatches) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum SinkWriteSemantics {
    ExactOnce,
    IdempotentAtLeastOnce,
    #[default]
    AtLeastOnce,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CompactionTransactionState {
    Pending,
    Sent,
    Acked,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WalPartRef {
    pub segment_id: String,
    pub source: SegmentSourceDescriptor,
    pub start: u64,
    pub len: u64,
    pub key: PartitionKey,
    pub cdc_meta_hash: Option<[u8; 32]>,
}

impl WalPartRef {
    pub fn identity_bytes(&self) -> Vec<u8> {
        wal_ref_identity_bytes(self)
    }

    pub fn to_runtime_ref(&self) -> RuntimeWalPartRef {
        RuntimeWalPartRef {
            segment_id: self.segment_id.clone(),
            source: self.source.stable_id(),
            start: self.start,
            len: self.len,
            sink_ref: self.key.sink_ref.clone(),
            namespace: self.key.namespace.clone(),
            partition: self.key.partition.clone(),
            time: self.key.time,
            schema_fingerprint: self.key.schema_fingerprint.clone(),
            cdc_meta_hash: self.cdc_meta_hash,
        }
    }
}

impl WalRefIdentity for WalPartRef {
    fn segment_id(&self) -> &str {
        &self.segment_id
    }

    fn source(&self) -> Cow<'_, str> {
        Cow::Owned(self.source.stable_id())
    }

    fn start(&self) -> u64 {
        self.start
    }

    fn len(&self) -> u64 {
        self.len
    }

    fn sink_ref(&self) -> &str {
        &self.key.sink_ref
    }

    fn namespace(&self) -> &str {
        &self.key.namespace
    }

    fn partition(&self) -> &str {
        &self.key.partition
    }

    fn time(&self) -> Option<i64> {
        self.key.time
    }

    fn schema_fingerprint(&self) -> &str {
        &self.key.schema_fingerprint
    }

    fn cdc_meta_hash(&self) -> Option<&[u8; 32]> {
        self.cdc_meta_hash.as_ref()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompactionTransaction {
    pub id: String,
    pub sink_ref: String,
    pub namespace: String,
    pub schema_fingerprint: String,
    pub write_policy: WritePolicy,
    pub semantics: SinkWriteSemantics,
    pub refs: Vec<WalPartRef>,
    pub target_filename: String,
    pub created_at_secs: u64,
    #[serde(default)]
    pub updated_at_secs: u64,
    #[serde(default)]
    pub attempts: u32,
    pub state: CompactionTransactionState,
}

impl CompactionTransaction {
    pub fn new(
        sink_ref: String,
        namespace: String,
        schema_fingerprint: String,
        write_policy: WritePolicy,
        semantics: SinkWriteSemantics,
        refs: Vec<WalPartRef>,
        target_filename: String,
    ) -> Self {
        let created_at_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let id = deterministic_compaction_id(
            &sink_ref,
            &namespace,
            &schema_fingerprint,
            write_policy,
            &refs,
        );
        Self {
            id,
            sink_ref,
            namespace,
            schema_fingerprint,
            write_policy,
            semantics,
            refs,
            target_filename,
            created_at_secs,
            updated_at_secs: created_at_secs,
            attempts: 0,
            state: CompactionTransactionState::Pending,
        }
    }

    pub fn with_state(mut self, state: CompactionTransactionState) -> Self {
        self.state = state;
        self.updated_at_secs = now_secs();
        self
    }

    pub fn mark_sent(mut self) -> Self {
        self.state = CompactionTransactionState::Sent;
        self.attempts = self.attempts.saturating_add(1);
        self.updated_at_secs = now_secs();
        self
    }

    pub fn mark_acked(mut self) -> Self {
        self.state = CompactionTransactionState::Acked;
        self.updated_at_secs = now_secs();
        self
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn deterministic_compaction_id(
    sink_ref: &str,
    namespace: &str,
    schema_fingerprint: &str,
    write_policy: WritePolicy,
    refs: &[WalPartRef],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"grouped-wal-compaction-v1");
    hasher.update(sink_ref.as_bytes());
    hasher.update([0]);
    hasher.update(namespace.as_bytes());
    hasher.update([0]);
    hasher.update(schema_fingerprint.as_bytes());
    hasher.update([0]);
    hasher.update(format!("{write_policy:?}").as_bytes());
    for identity in canonical_sorted_wal_ref_identity_bytes(refs) {
        hasher.update(identity);
        hasher.update([0xff]);
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_for(segment_id: &str, start: u64) -> WalPartRef {
        WalPartRef {
            segment_id: segment_id.to_string(),
            source: SegmentSourceDescriptor::Disk {
                path: PathBuf::from(format!("/tmp/{segment_id}.seg")),
            },
            start,
            len: 10,
            key: PartitionKey {
                sink_ref: "sink.main".to_string(),
                namespace: "ns".to_string(),
                partition: "p=1".to_string(),
                time: Some(1),
                schema_fingerprint: "schema".to_string(),
            },
            cdc_meta_hash: None,
        }
    }

    #[test]
    fn deterministic_id_is_independent_of_ref_order() {
        let refs_a = vec![ref_for("b", 2), ref_for("a", 1)];
        let refs_b = vec![ref_for("a", 1), ref_for("b", 2)];
        let id =
            deterministic_compaction_id("sink.main", "ns", "schema", WritePolicy::Append, &refs_a);
        assert_eq!(
            id,
            deterministic_compaction_id("sink.main", "ns", "schema", WritePolicy::Append, &refs_b)
        );
        assert_eq!(
            id,
            "6fb2bcdd0952a17b0260bbed6eac9cd75e5ef09d8dbfd5c0894a2a7bee09ec39"
        );
    }

    #[test]
    fn deterministic_replay_id_is_stable_across_ref_order_with_cdc_metadata() {
        let mut first = ref_for("segment-a", 1);
        first.cdc_meta_hash = Some([0x11; 32]);
        let mut second = ref_for("segment-b", 2);
        second.cdc_meta_hash = Some([0x22; 32]);

        let replay_a = CompactionTransaction::new(
            "sink.main".to_string(),
            "ns".to_string(),
            "schema".to_string(),
            WritePolicy::Append,
            SinkWriteSemantics::ExactOnce,
            vec![first.clone(), second.clone()],
            "out.parquet".to_string(),
        );
        let replay_b = CompactionTransaction::new(
            "sink.main".to_string(),
            "ns".to_string(),
            "schema".to_string(),
            WritePolicy::Append,
            SinkWriteSemantics::ExactOnce,
            vec![second, first],
            "out.parquet".to_string(),
        );

        assert_eq!(replay_a.id, replay_b.id);
        assert_eq!(
            replay_a.refs[0].to_runtime_ref().cdc_meta_hash,
            Some([0x11; 32])
        );
    }

    #[test]
    fn deterministic_replay_id_changes_with_cdc_metadata() {
        let mut original = ref_for("segment-a", 1);
        original.cdc_meta_hash = Some([0x11; 32]);
        let mut changed = original.clone();
        changed.cdc_meta_hash = Some([0x12; 32]);

        assert_ne!(
            deterministic_compaction_id(
                "sink.main",
                "ns",
                "schema",
                WritePolicy::Append,
                &[original],
            ),
            deterministic_compaction_id(
                "sink.main",
                "ns",
                "schema",
                WritePolicy::Append,
                &[changed],
            )
        );
    }
}
