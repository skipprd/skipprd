//! S3 WAL persist: encode once, PUT body only, reconcile the same snapshot
//! ID on timeout, never restore live bytes, never mint a second ID on a
//! fenced writer lease. Catalog commit is the ownership record.

use std::collections::HashMap;
use std::io;
use std::time::SystemTime;

use skippr_lease::LeaseGuard;
use tracing::error;

use crate::buffer::segment_file::{encode_snapshot, PartitionKey, SegmentFile, WalPayload};
use crate::buffer::wal_object_store::{snapshot_body_key, GetOutcome, PutOutcome, WalObjectStore};
use crate::buffer::wal_store::{SegmentWriteLocation, SegmentWriteResult};
use crate::helpers::offsets::OffsetKey;

/// GET-admit attempts after an Unknown PUT. Bound so the operation stays
/// inside the pipeline writer lease window.
pub const RECONCILE_GET_ATTEMPTS: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistStage {
    Body,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PersistError {
    Definite {
        snapshot_id: String,
        stage: PersistStage,
        message: String,
    },
    Unreconciled {
        snapshot_id: String,
        stage: PersistStage,
        message: String,
    },
    Fenced {
        snapshot_id: String,
        message: String,
    },
}

impl PersistError {
    pub fn snapshot_id(&self) -> &str {
        match self {
            Self::Definite { snapshot_id, .. }
            | Self::Unreconciled { snapshot_id, .. }
            | Self::Fenced { snapshot_id, .. } => snapshot_id,
        }
    }

    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Unreconciled { .. } | Self::Fenced { .. })
    }
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Definite {
                snapshot_id,
                stage,
                message,
            } => write!(
                f,
                "s3 wal persist definite failure id={snapshot_id} stage={stage:?}: {message}"
            ),
            Self::Unreconciled {
                snapshot_id,
                stage,
                message,
            } => write!(
                f,
                "s3 wal persist unreconciled id={snapshot_id} stage={stage:?}: {message}"
            ),
            Self::Fenced {
                snapshot_id,
                message,
            } => write!(f, "s3 wal persist fenced id={snapshot_id}: {message}"),
        }
    }
}

impl std::error::Error for PersistError {}

impl From<PersistError> for io::Error {
    fn from(err: PersistError) -> Self {
        io::Error::other(err)
    }
}

fn fence_on_fatal(lease: &LeaseGuard, result: &Result<SegmentWriteResult, PersistError>) {
    if let Err(err) = result {
        if err.is_fatal() {
            lease.fence();
        }
    }
}

fn require_epoch(
    lease: &LeaseGuard,
    snapshot_id: &str,
) -> Result<skippr_lease::LeaseEpoch, PersistError> {
    lease
        .require_active_epoch()
        .map_err(|err| PersistError::Fenced {
            snapshot_id: snapshot_id.to_string(),
            message: err.to_string(),
        })
}

fn require_same_epoch(
    lease: &LeaseGuard,
    snapshot_id: &str,
    expected: skippr_lease::LeaseEpoch,
) -> Result<(), PersistError> {
    lease
        .require_same_active_epoch(expected)
        .map_err(|err| PersistError::Fenced {
            snapshot_id: snapshot_id.to_string(),
            message: err.to_string(),
        })
}

async fn reconcile_get(store: &dyn WalObjectStore, key: &str) -> GetOutcome {
    let mut last = GetOutcome::Unknown;
    for _ in 0..RECONCILE_GET_ATTEMPTS {
        last = store.get(key).await;
        if !matches!(last, GetOutcome::Unknown) {
            return last;
        }
    }
    last
}

async fn put_or_reconcile(
    store: &dyn WalObjectStore,
    key: &str,
    body: Vec<u8>,
    snapshot_id: &str,
    stage: PersistStage,
) -> Result<(Vec<u8>, bool), PersistError> {
    match store.put(key, body.clone()).await {
        PutOutcome::Applied => Ok((body, false)),
        PutOutcome::DefiniteFailure(message) => Err(PersistError::Definite {
            snapshot_id: snapshot_id.to_string(),
            stage,
            message,
        }),
        PutOutcome::Unknown => match reconcile_get(store, key).await {
            GetOutcome::Found(found) if found == body => Ok((found, true)),
            GetOutcome::Found(_) => Err(PersistError::Unreconciled {
                snapshot_id: snapshot_id.to_string(),
                stage,
                message: format!("{key} exists with unexpected bytes"),
            }),
            GetOutcome::Missing => Err(PersistError::Definite {
                snapshot_id: snapshot_id.to_string(),
                stage,
                message: format!("{key} timed out and was never applied"),
            }),
            GetOutcome::Unknown => Err(PersistError::Unreconciled {
                snapshot_id: snapshot_id.to_string(),
                stage,
                message: format!("{key} GET reconciliation exhausted"),
            }),
        },
    }
}

pub async fn persist_snapshot_pair(
    store: &dyn WalObjectStore,
    prefix_url: &str,
    snapshot_id: &str,
    offsets: &HashMap<OffsetKey, u64>,
    batches: &HashMap<PartitionKey, WalPayload>,
    partitions_meta: &HashMap<PartitionKey, (u64, SystemTime)>,
    part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
    lease: &LeaseGuard,
    created_at: SystemTime,
) -> Result<SegmentWriteResult, PersistError> {
    let epoch = require_epoch(lease, snapshot_id)?;
    let (bucket, body_key) =
        snapshot_body_key(prefix_url, snapshot_id).map_err(|err| PersistError::Definite {
            snapshot_id: snapshot_id.to_string(),
            stage: PersistStage::Body,
            message: err.to_string(),
        })?;
    persist_snapshot_keys(
        store,
        &bucket,
        &body_key,
        snapshot_id,
        offsets,
        batches,
        partitions_meta,
        part_meta_blobs,
        lease,
        epoch,
        created_at,
    )
    .await
}

pub async fn persist_snapshot_keys(
    store: &dyn WalObjectStore,
    bucket: &str,
    body_key: &str,
    snapshot_id: &str,
    offsets: &HashMap<OffsetKey, u64>,
    batches: &HashMap<PartitionKey, WalPayload>,
    partitions_meta: &HashMap<PartitionKey, (u64, SystemTime)>,
    part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
    lease: &LeaseGuard,
    epoch: skippr_lease::LeaseEpoch,
    created_at: SystemTime,
) -> Result<SegmentWriteResult, PersistError> {
    let result = persist_payload_object(
        store,
        bucket,
        body_key,
        snapshot_id,
        offsets,
        batches,
        partitions_meta,
        part_meta_blobs,
        lease,
        epoch,
        created_at,
    )
    .await;
    fence_on_fatal(lease, &result);
    result
}

async fn persist_payload_object(
    store: &dyn WalObjectStore,
    bucket: &str,
    body_key: &str,
    snapshot_id: &str,
    offsets: &HashMap<OffsetKey, u64>,
    batches: &HashMap<PartitionKey, WalPayload>,
    partitions_meta: &HashMap<PartitionKey, (u64, SystemTime)>,
    part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
    lease: &LeaseGuard,
    epoch: skippr_lease::LeaseEpoch,
    created_at: SystemTime,
) -> Result<SegmentWriteResult, PersistError> {
    require_same_epoch(lease, snapshot_id, epoch)?;
    let encoded = encode_snapshot(
        offsets,
        batches,
        partitions_meta,
        part_meta_blobs,
        created_at,
    )
    .map_err(|err| PersistError::Definite {
        snapshot_id: snapshot_id.to_string(),
        stage: PersistStage::Body,
        message: err.to_string(),
    })?;
    let (body_bytes, body_reconciled) = put_or_reconcile(
        store,
        body_key,
        encoded.bytes.clone(),
        snapshot_id,
        PersistStage::Body,
    )
    .await?;
    let _ = body_reconciled;
    require_same_epoch(lease, snapshot_id, epoch)?;
    let meta = SegmentFile::read_metadata_from_bytes(&body_bytes).map_err(|err| {
        PersistError::Unreconciled {
            snapshot_id: snapshot_id.to_string(),
            stage: PersistStage::Body,
            message: format!("payload metadata failed after publication: {err}"),
        }
    })?;
    require_same_epoch(lease, snapshot_id, epoch)?;
    Ok(SegmentWriteResult {
        meta,
        total_rows: encoded.total_rows,
        sha256: encoded.sha256,
        location: SegmentWriteLocation::S3 {
            key: body_key.to_string(),
            bucket: bucket.to_string(),
        },
        offsets_published: false,
    })
}

pub fn persist_error_from_io(err: PersistError, snapshot_id: &str) -> io::Error {
    error!(
        "s3 wal persist failed id={} err={} fatal={}",
        snapshot_id,
        err,
        err.is_fatal()
    );
    io::Error::from(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::wal_object_store::{ScriptedGet, ScriptedObjectStore, ScriptedPut};
    use arrow::array::{Int32Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use serial_test::serial;
    use skippr_lease::{
        Clock, LeaseEpoch, LeaseGuard, LeaseSession, NodeId, PipelineKey, SystemClock,
    };
    use std::sync::Arc;
    use std::time::Duration;

    fn batch() -> RecordBatch {
        let schema = Schema::new(vec![Field::new("v", DataType::Int32, false)]);
        let arr = Int32Array::from(vec![1, 2, 3]);
        RecordBatch::try_new(Arc::new(schema), vec![Arc::new(arr)]).unwrap()
    }

    fn snapshot_parts() -> (
        HashMap<OffsetKey, u64>,
        HashMap<PartitionKey, WalPayload>,
        HashMap<PartitionKey, (u64, SystemTime)>,
        HashMap<PartitionKey, Vec<u8>>,
    ) {
        let key = PartitionKey {
            sink_ref: "sink".into(),
            namespace: "ns".into(),
            partition: "p".into(),
            time: Some(0),
            schema_fingerprint: "fp".into(),
        };
        let mut batches = HashMap::new();
        batches.insert(key.clone(), WalPayload::Arrow(vec![batch()]));
        let mut meta = HashMap::new();
        meta.insert(key, (12, SystemTime::now()));
        let mut offsets = HashMap::new();
        offsets.insert(OffsetKey::new("ns", "src-1"), 7);
        (offsets, batches, meta, HashMap::new())
    }

    fn test_guard() -> Arc<LeaseGuard> {
        let key = PipelineKey::new("t", "w", "p").expect("pipeline key");
        LeaseGuard::single_node(key, Arc::new(SystemClock::new()))
    }

    async fn persist(
        store: &dyn WalObjectStore,
        id: &str,
        lease: Option<&LeaseGuard>,
    ) -> Result<SegmentWriteResult, PersistError> {
        let fallback = test_guard();
        let lease = lease.unwrap_or(fallback.as_ref());
        let (offsets, batches, meta, blobs) = snapshot_parts();
        persist_snapshot_keys(
            store,
            "bucket",
            &format!("{id}.seg"),
            id,
            &offsets,
            &batches,
            &meta,
            &blobs,
            lease,
            lease
                .require_active_epoch()
                .map_err(|err| PersistError::Fenced {
                    snapshot_id: id.to_string(),
                    message: err.to_string(),
                })?,
            SystemTime::UNIX_EPOCH,
        )
        .await
    }

    async fn persist_err(
        store: &dyn WalObjectStore,
        id: &str,
        lease: Option<&LeaseGuard>,
    ) -> PersistError {
        match persist(store, id, lease).await {
            Ok(_) => panic!("expected persist error for {id}"),
            Err(err) => err,
        }
    }

    #[tokio::test]
    #[serial]
    async fn applied_body_acks_same_id() {
        let store = ScriptedObjectStore::new();
        let result = persist(&store, "snap-ok", None).await.unwrap();
        assert!(store.inner().contains("snap-ok.seg"));
        assert!(!store.inner().contains("snap-ok.seg.commit"));
        assert_eq!(result.location_key(), "snap-ok.seg");
    }

    #[tokio::test]
    #[serial]
    async fn lost_body_response_reconciles_same_id() {
        let store = ScriptedObjectStore::new();
        store.script_put("snap-lost-body.seg", vec![ScriptedPut::ApplyLostResponse]);
        let result = persist(&store, "snap-lost-body", None).await.unwrap();
        assert_eq!(result.location_key(), "snap-lost-body.seg");
        assert!(store.inner().contains("snap-lost-body.seg"));
        assert!(!store.inner().contains("snap-lost-body.seg.commit"));
    }

    #[tokio::test]
    #[serial]
    async fn never_landed_body_is_definite_and_does_not_restore() {
        let store = ScriptedObjectStore::new();
        store.script_put("snap-miss-body.seg", vec![ScriptedPut::NeverLand]);
        let err = persist_err(&store, "snap-miss-body", None).await;
        assert!(matches!(
            err,
            PersistError::Definite {
                stage: PersistStage::Body,
                ..
            }
        ));
        assert_eq!(err.snapshot_id(), "snap-miss-body");
        assert!(!store.inner().contains("snap-miss-body.seg"));
        assert!(!store.inner().contains("snap-miss-body.seg.commit"));
        assert!(!err.is_fatal());
    }

    #[tokio::test]
    #[serial]
    async fn get_failure_after_unknown_body_put_is_fatal_and_blocks_second_id() {
        let guard = test_guard();
        let store = ScriptedObjectStore::new();
        store.script_put("snap-unk-body.seg", vec![ScriptedPut::ApplyLostResponse]);
        store.script_get("snap-unk-body.seg", ScriptedGet::AlwaysFail);
        let err = persist_err(&store, "snap-unk-body", Some(guard.as_ref())).await;
        assert!(matches!(err, PersistError::Unreconciled { .. }));
        assert!(err.is_fatal());
        assert!(guard.require_active_epoch().is_err());
        let second = persist_err(&store, "snap-other-body", Some(guard.as_ref())).await;
        assert_eq!(second.snapshot_id(), "snap-other-body");
        assert!(matches!(second, PersistError::Fenced { .. }));
        assert!(!store.inner().contains("snap-other-body.seg"));
    }

    #[tokio::test]
    #[serial]
    async fn get_retry_then_admit_same_id() {
        let store = ScriptedObjectStore::new();
        store.script_put("snap-retry.seg", vec![ScriptedPut::ApplyLostResponse]);
        store.script_get("snap-retry.seg", ScriptedGet::FailOnceThenLive);
        let result = persist(&store, "snap-retry", None).await.unwrap();
        assert_eq!(result.location_key(), "snap-retry.seg");
    }

    #[tokio::test]
    #[serial]
    async fn stale_epoch_cannot_ack() {
        let store = ScriptedObjectStore::new();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let clock = Arc::new(SystemClock::new());
        let session = LeaseSession {
            owner: NodeId::generate(),
            epoch: LeaseEpoch::new(1),
            heartbeat: 1,
            initialized: true,
            local_deadline: clock.monotonic_now() + Duration::from_secs(30),
        };
        let guard = LeaseGuard::owner_elect(key, session, clock);
        guard.activate(guard.leased_session().unwrap()).unwrap();
        persist(&store, "snap-lease", Some(guard.as_ref()))
            .await
            .unwrap();
        guard.fence();
        let err = persist_err(&store, "snap-stale", Some(guard.as_ref())).await;
        assert!(matches!(err, PersistError::Fenced { .. }));
        assert!(!store.inner().contains("snap-stale.seg"));
    }

    #[test]
    fn persist_is_the_only_s3_writer() {
        let persist = include_str!("wal_persist.rs");
        let object_store = include_str!("wal_object_store.rs");
        assert!(persist.contains("persist_snapshot_pair"));
        assert!(persist.contains("lease: &LeaseGuard"));
        assert!(!object_store.contains(concat!("reclaim_owned", "_pair")));
        assert!(!persist.contains(concat!("WAL_WRITER", "_FATAL")));
        assert!(!persist.contains(concat!("wal_is", "_fatal")));
        assert!(!persist.contains(concat!("mark_wal", "_fatal")));
        let writer = include_str!("wal_writer.rs");
        assert!(!writer.contains(concat!("WAL_WRITER", "_FATAL")));
    }
}

impl SegmentWriteResult {
    #[cfg(test)]
    fn location_key(&self) -> &str {
        match &self.location {
            SegmentWriteLocation::S3 { key, .. } => key,
            SegmentWriteLocation::Disk { path } => path.to_str().unwrap_or(""),
            SegmentWriteLocation::Clustered { path, .. } => path.to_str().unwrap_or(""),
        }
    }
}
