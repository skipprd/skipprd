use crate::buffer::segment_file::PartitionKey;
use crate::helpers::configuration::Config;
use crate::plugins::source_contract::WritePolicy;
use crate::runtime_plugins::protocol::RuntimeWalPartRef;
use crate::sink_apply_identity::{
    canonical_sorted_wal_ref_identity_bytes, wal_ref_identity_bytes, WalRefIdentity,
};
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::warn;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SegmentSourceDescriptor {
    Disk { path: PathBuf },
    S3 { bucket: String, key: String },
}

impl SegmentSourceDescriptor {
    pub fn stable_id(&self) -> String {
        match self {
            Self::Disk { path } => path.to_string_lossy().to_string(),
            Self::S3 { bucket, key } => format!("s3://{bucket}/{key}"),
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
    Tombstoned,
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

pub fn manifest_dir() -> PathBuf {
    PathBuf::from(format!(
        "{}/segment_buffer/compactions",
        Config::get_data_dir()
    ))
}

pub fn manifest_path_for(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

pub fn persist_manifest(txn: &CompactionTransaction) -> io::Result<()> {
    let dir = manifest_dir();
    fs::create_dir_all(&dir)?;
    let path = manifest_path_for(&dir, &txn.id);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(txn).map_err(io::Error::other)?;
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)
}

pub fn remove_manifest(id: &str) -> io::Result<()> {
    let path = manifest_path_for(&manifest_dir(), id);
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn load_manifest_file(path: &Path) -> Option<CompactionTransaction> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!(
                "Compactor: failed to read compaction manifest {:?}: {}",
                path, err
            );
            return None;
        }
    };
    if bytes.is_empty() {
        warn!("Compactor: removing empty compaction manifest {:?}", path);
        if let Err(err) = fs::remove_file(path) {
            warn!(
                "Compactor: failed to remove empty compaction manifest {:?}: {}",
                path, err
            );
        }
        return None;
    }
    match serde_json::from_slice::<CompactionTransaction>(&bytes) {
        Ok(txn) => Some(txn),
        Err(err) => {
            warn!(
                "Compactor: removing corrupt compaction manifest {:?}: {}",
                path, err
            );
            if let Err(remove_err) = fs::remove_file(path) {
                warn!(
                    "Compactor: failed to remove corrupt compaction manifest {:?}: {}",
                    path, remove_err
                );
            }
            None
        }
    }
}

pub fn load_pending_manifests() -> io::Result<Vec<CompactionTransaction>> {
    let dir = manifest_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let now = now_secs();
    let sent_stale_secs = Config::getenv("WAL_COMPACTION_SENT_STALE_SECS", "300")
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or(300);
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(mut txn) = load_manifest_file(&path) else {
            continue;
        };
        if txn.updated_at_secs == 0 {
            txn.updated_at_secs = txn.created_at_secs;
        }
        if matches!(txn.state, CompactionTransactionState::Sent)
            && now.saturating_sub(txn.updated_at_secs) < sent_stale_secs
        {
            continue;
        }
        if matches!(txn.state, CompactionTransactionState::Sent) {
            warn!(
                "Compactor: retrying stale sent manifest id={} sink_ref={} namespace={} age_secs={} attempts={}",
                txn.id,
                txn.sink_ref,
                txn.namespace,
                now.saturating_sub(txn.updated_at_secs),
                txn.attempts,
            );
            txn.state = CompactionTransactionState::Pending;
            txn.updated_at_secs = now;
        }
        if !matches!(txn.state, CompactionTransactionState::Tombstoned) {
            out.push(txn);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

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

    #[test]
    #[serial]
    fn load_pending_manifests_skips_empty_and_corrupt_files() {
        use crate::helpers::configuration::Config;
        use std::sync::{Mutex, MutexGuard, OnceLock};

        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        fn env_lock() -> MutexGuard<'static, ()> {
            ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
        }

        let _guard = env_lock();
        Config::reset_envcache();
        let temp = tempfile::tempdir().unwrap();
        let old_data_dir = std::env::var("DATA_DIR").ok();
        let old_root = std::env::var("SKIPPR_PIPELINE_DATA_ROOT").ok();
        Config::setenv("DATA_DIR", temp.path().to_str().unwrap());
        Config::setenv("SKIPPR_PIPELINE_DATA_ROOT", "true");

        let comp_dir = manifest_dir();
        fs::create_dir_all(&comp_dir).unwrap();

        let good = CompactionTransaction::new(
            "sink.main".to_string(),
            "ns".to_string(),
            "schema".to_string(),
            WritePolicy::Append,
            SinkWriteSemantics::AtLeastOnce,
            vec![ref_for("good", 1)],
            "out.parquet".to_string(),
        );
        persist_manifest(&good).unwrap();
        fs::write(comp_dir.join("empty.json"), b"").unwrap();
        fs::write(comp_dir.join("corrupt.json"), b"{not-json").unwrap();

        let loaded = load_pending_manifests().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, good.id);
        assert!(!comp_dir.join("empty.json").exists());
        assert!(!comp_dir.join("corrupt.json").exists());
        assert!(comp_dir.join(format!("{}.json", good.id)).exists());

        if let Some(value) = old_data_dir {
            Config::setenv("DATA_DIR", &value);
        } else {
            std::env::remove_var("DATA_DIR");
            Config::set_evncache("DATA_DIR", "");
        }
        if let Some(value) = old_root {
            Config::setenv("SKIPPR_PIPELINE_DATA_ROOT", &value);
        } else {
            std::env::remove_var("SKIPPR_PIPELINE_DATA_ROOT");
            Config::set_evncache("SKIPPR_PIPELINE_DATA_ROOT", "");
        }
    }
}
