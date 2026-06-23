use crate::buffer::segment_file::PartitionKey;
use crate::helpers::configuration::Config;
use crate::plugins::source_contract::WritePolicy;
use crate::runtime_plugins::protocol::RuntimeWalPartRef;
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum SinkGroupingSupport {
    #[default]
    None,
    AppendOnlyBatches,
    CdcEncodedBatches,
    FinalStateBatches,
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
        let mut out = Vec::new();
        out.extend_from_slice(self.segment_id.as_bytes());
        out.push(0);
        out.extend_from_slice(self.source.stable_id().as_bytes());
        out.push(0);
        out.extend_from_slice(&self.start.to_le_bytes());
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(self.key.sink_ref.as_bytes());
        out.push(0);
        out.extend_from_slice(self.key.namespace.as_bytes());
        out.push(0);
        out.extend_from_slice(self.key.partition.as_bytes());
        out.push(0);
        out.extend_from_slice(&self.key.time.unwrap_or(0).to_le_bytes());
        out.extend_from_slice(self.key.schema_fingerprint.as_bytes());
        if let Some(hash) = self.cdc_meta_hash {
            out.extend_from_slice(&hash);
        }
        out
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
            state: CompactionTransactionState::Pending,
        }
    }

    pub fn with_state(mut self, state: CompactionTransactionState) -> Self {
        self.state = state;
        self
    }
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
    let mut identities = refs
        .iter()
        .map(WalPartRef::identity_bytes)
        .collect::<Vec<_>>();
    identities.sort();
    for identity in identities {
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

pub fn load_pending_manifests() -> io::Result<Vec<CompactionTransaction>> {
    let dir = manifest_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path)?;
        let txn =
            serde_json::from_slice::<CompactionTransaction>(&bytes).map_err(io::Error::other)?;
        if !matches!(txn.state, CompactionTransactionState::Tombstoned) {
            out.push(txn);
        }
    }
    Ok(out)
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
        assert_eq!(
            deterministic_compaction_id("sink.main", "ns", "schema", WritePolicy::Append, &refs_a),
            deterministic_compaction_id("sink.main", "ns", "schema", WritePolicy::Append, &refs_b)
        );
    }
}
