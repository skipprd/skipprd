use std::fmt;
use std::io;

use async_trait::async_trait;
use serde_derive::{Deserialize, Serialize};

use crate::protocol::RuntimeWalPartRef;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObjectWriteManifest {
    pub compaction_id: String,
    pub idempotency_key: String,
    pub schema_fingerprint: String,
    pub wal_refs_fingerprint: String,
    pub wal_ref_count: usize,
}

impl ObjectWriteManifest {
    pub fn from_context(
        compaction_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        schema_fingerprint: impl Into<String>,
        wal_refs: &[RuntimeWalPartRef],
    ) -> Self {
        Self {
            compaction_id: compaction_id.into(),
            idempotency_key: idempotency_key.into(),
            schema_fingerprint: schema_fingerprint.into(),
            wal_refs_fingerprint: wal_refs_fingerprint(wal_refs),
            wal_ref_count: wal_refs.len(),
        }
    }

    pub fn grouped_receipt_object_key(
        root_prefix: &str,
        namespace: &str,
        compaction_id: &str,
    ) -> String {
        sidecar_manifest_object_key(root_prefix, namespace, compaction_id)
    }

    pub fn matches_context(
        &self,
        compaction_id: &str,
        idempotency_key: &str,
        schema_fingerprint: &str,
        wal_refs: &[RuntimeWalPartRef],
    ) -> bool {
        self.compaction_id == compaction_id
            && self.idempotency_key == idempotency_key
            && self.schema_fingerprint == schema_fingerprint
            && self.wal_refs_fingerprint == wal_refs_fingerprint(wal_refs)
            && self.wal_ref_count == wal_refs.len()
    }

    pub fn to_json_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).map_err(|err| io::Error::other(err.to_string()))
    }

    pub fn from_json_bytes(bytes: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(bytes).map_err(|err| io::Error::other(err.to_string()))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GroupedWriteReceipt {
    #[serde(default = "grouped_write_receipt_version")]
    pub version: u32,
    pub compaction_id: String,
    pub idempotency_key: String,
    pub schema_fingerprint: String,
    pub wal_refs_fingerprint: String,
    pub wal_ref_count: usize,
    pub final_s3_key: String,
    pub etag: String,
    #[serde(default)]
    pub checksum: Option<String>,
    pub rows: u64,
    pub bytes: u64,
    pub transport_chunk_count: u32,
    pub completed_at_unix_secs: u64,
}

fn grouped_write_receipt_version() -> u32 {
    2
}

impl GroupedWriteReceipt {
    pub fn from_manifest_and_upload(
        manifest: &ObjectWriteManifest,
        final_s3_key: impl Into<String>,
        etag: impl Into<String>,
        checksum: Option<String>,
        rows: u64,
        bytes: u64,
        transport_chunk_count: u32,
    ) -> Self {
        let completed_at_unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            version: grouped_write_receipt_version(),
            compaction_id: manifest.compaction_id.clone(),
            idempotency_key: manifest.idempotency_key.clone(),
            schema_fingerprint: manifest.schema_fingerprint.clone(),
            wal_refs_fingerprint: manifest.wal_refs_fingerprint.clone(),
            wal_ref_count: manifest.wal_ref_count,
            final_s3_key: final_s3_key.into(),
            etag: etag.into(),
            checksum,
            rows,
            bytes,
            transport_chunk_count,
            completed_at_unix_secs,
        }
    }

    pub fn matches_manifest(&self, manifest: &ObjectWriteManifest) -> bool {
        manifest.compaction_id == self.compaction_id
            && manifest.idempotency_key == self.idempotency_key
            && manifest.schema_fingerprint == self.schema_fingerprint
            && manifest.wal_refs_fingerprint == self.wal_refs_fingerprint
            && manifest.wal_ref_count == self.wal_ref_count
    }

    pub fn to_json_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec_pretty(self).map_err(|err| io::Error::other(err.to_string()))
    }

    pub fn from_json_bytes(bytes: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(bytes).map_err(|err| io::Error::other(err.to_string()))
    }
}

pub fn legacy_chunk_idempotency_key(compaction_id: &str, chunk_index: u64) -> String {
    format!("{compaction_id}-chunk-{chunk_index:08}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdempotentWriteDecision {
    AlreadyApplied,
    Proceed,
}

pub fn deterministic_object_name(idempotency_key: &str, extension: &str) -> io::Result<String> {
    if idempotency_key.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "idempotent object write requires a non-empty idempotency key",
        ));
    }
    let extension = extension.trim_start_matches('.');
    Ok(if extension.is_empty() {
        idempotency_key.to_string()
    } else {
        format!("{idempotency_key}.{extension}")
    })
}

pub fn manifest_object_name(data_object_name: &str) -> String {
    format!("{data_object_name}.skippr-manifest.json")
}

pub fn sidecar_manifest_object_key(
    root_prefix: &str,
    namespace: &str,
    data_object_name: &str,
) -> String {
    let root_prefix = root_prefix.trim_matches('/');
    let namespace = namespace.trim_matches('/');
    let manifest_name = manifest_object_name(data_object_name);
    let mut parts = Vec::new();
    if !root_prefix.is_empty() {
        parts.push(root_prefix);
    }
    parts.push("_skippr-idempotency");
    if !namespace.is_empty() {
        parts.push(namespace);
    }
    parts.push(&manifest_name);
    parts.join("/")
}

pub fn wal_refs_fingerprint(wal_refs: &[RuntimeWalPartRef]) -> String {
    let mut identities = wal_refs
        .iter()
        .map(|wal_ref| {
            format!(
                "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{:?}\0{}",
                wal_ref.segment_id,
                wal_ref.source,
                wal_ref.start,
                wal_ref.len,
                wal_ref.sink_ref,
                wal_ref.namespace,
                wal_ref.partition,
                wal_ref.time,
                wal_ref.schema_fingerprint
            )
        })
        .collect::<Vec<_>>();
    identities.sort();
    identities.join("\u{1f}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LedgerOutcome {
    Apply(ApplyPermit),
    AlreadyApplied,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyPermit {
    pub idempotency_key: String,
}

impl fmt::Display for ApplyPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.idempotency_key)
    }
}

#[async_trait]
pub trait SinkIdempotencyLedger: Send + Sync {
    async fn begin_apply(&self, idempotency_key: &str) -> io::Result<LedgerOutcome>;
    async fn mark_applied(&self, permit: ApplyPermit) -> io::Result<()>;
    async fn release(&self, permit: ApplyPermit) -> io::Result<()>;
}

pub fn cdc_message_envelope(
    compaction_id: &str,
    wal_ref: Option<&RuntimeWalPartRef>,
    order_token: Option<&str>,
    row: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "compaction_id": compaction_id,
        "wal_ref": wal_ref,
        "order_token": order_token,
        "row": row,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wal_ref(segment_id: &str, start: u64) -> RuntimeWalPartRef {
        RuntimeWalPartRef {
            segment_id: segment_id.to_string(),
            source: format!("s3://bucket/{segment_id}"),
            start,
            len: 10,
            sink_ref: "sink.main".to_string(),
            namespace: "ns".to_string(),
            partition: "p=1".to_string(),
            time: Some(1),
            schema_fingerprint: "schema".to_string(),
            cdc_meta_hash: None,
        }
    }

    #[test]
    fn object_manifest_matches_same_context_independent_of_ref_order() {
        let refs = vec![wal_ref("b", 2), wal_ref("a", 1)];
        let reversed = vec![wal_ref("a", 1), wal_ref("b", 2)];
        let manifest = ObjectWriteManifest::from_context("c1", "k1", "schema", &refs);
        assert!(manifest.matches_context("c1", "k1", "schema", &reversed));
    }

    #[test]
    fn object_manifest_rejects_schema_mismatch() {
        let refs = vec![wal_ref("a", 1)];
        let manifest = ObjectWriteManifest::from_context("c1", "k1", "schema-a", &refs);
        assert!(!manifest.matches_context("c1", "k1", "schema-b", &refs));
    }

    #[test]
    fn sidecar_manifest_key_stays_out_of_table_prefix() {
        assert_eq!(
            sidecar_manifest_object_key("deadletters-archive", "_dl_events", "abc123"),
            "deadletters-archive/_skippr-idempotency/_dl_events/abc123.skippr-manifest.json"
        );
        assert_eq!(
            sidecar_manifest_object_key("/root/", "/events/", "abc123"),
            "root/_skippr-idempotency/events/abc123.skippr-manifest.json"
        );
    }
}
