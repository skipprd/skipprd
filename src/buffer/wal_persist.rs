//! S3 WAL persist: encode once, PUT body then SEGC commit, reconcile the
//! same snapshot ID on timeout, never restore live bytes, never mint a
//! second ID in this process.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use arrow::array::RecordBatch;
use skippr_lease::LeaseGuard;
use tracing::error;

use crate::buffer::segment_file::{encode_snapshot, PartitionKey, SegmentFile};
use crate::buffer::wal_object_store::{snapshot_pair_keys, GetOutcome, PutOutcome, WalObjectStore};
use crate::buffer::wal_store::{SegmentWriteLocation, SegmentWriteResult};
use crate::helpers::offsets::OffsetKey;

/// GET-admit attempts after an Unknown PUT. Bound so the operation stays
/// inside the pipeline writer lease window.
pub const RECONCILE_GET_ATTEMPTS: u32 = 2;

static WAL_FATAL: AtomicBool = AtomicBool::new(false);

pub fn wal_is_fatal() -> bool {
    WAL_FATAL.load(Ordering::SeqCst)
}

pub fn mark_wal_fatal() {
    WAL_FATAL.store(true, Ordering::SeqCst);
}

#[cfg(test)]
pub fn reset_wal_fatal() {
    WAL_FATAL.store(false, Ordering::SeqCst);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistStage {
    Body,
    Commit,
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
        if err.is_fatal() {
            mark_wal_fatal();
        }
        io::Error::other(err.to_string())
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
            GetOutcome::Unknown => {
                mark_wal_fatal();
                Err(PersistError::Unreconciled {
                    snapshot_id: snapshot_id.to_string(),
                    stage,
                    message: format!("{key} GET reconciliation exhausted"),
                })
            }
        },
    }
}

async fn get_required(
    store: &dyn WalObjectStore,
    key: &str,
    snapshot_id: &str,
    stage: PersistStage,
) -> Result<Vec<u8>, PersistError> {
    match reconcile_get(store, key).await {
        GetOutcome::Found(bytes) => Ok(bytes),
        GetOutcome::Missing => Err(PersistError::Definite {
            snapshot_id: snapshot_id.to_string(),
            stage,
            message: format!("{key} missing while admitting timed-out pair"),
        }),
        GetOutcome::Unknown => {
            mark_wal_fatal();
            Err(PersistError::Unreconciled {
                snapshot_id: snapshot_id.to_string(),
                stage,
                message: format!("{key} GET unknown while admitting timed-out pair"),
            })
        }
    }
}

async fn admit_pair_from_store(
    store: &dyn WalObjectStore,
    body_key: &str,
    commit_key: &str,
    snapshot_id: &str,
) -> Result<crate::buffer::segment_file::SegmentFileMetadata, PersistError> {
    let body = get_required(store, body_key, snapshot_id, PersistStage::Body).await?;
    let commit = get_required(store, commit_key, snapshot_id, PersistStage::Commit).await?;
    crate::buffer::segment_file::SegmentFile::admit_owned_pair_bytes(&body, &commit).map_err(
        |err| PersistError::Unreconciled {
            snapshot_id: snapshot_id.to_string(),
            stage: PersistStage::Commit,
            message: format!("bound pair failed after timeout reconcile: {err}"),
        },
    )
}

pub async fn persist_snapshot_pair(
    store: &dyn WalObjectStore,
    prefix_url: &str,
    snapshot_id: &str,
    offsets: &HashMap<OffsetKey, u64>,
    batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
    partitions_meta: &HashMap<PartitionKey, (u64, SystemTime)>,
    part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
    lease: &LeaseGuard,
) -> Result<SegmentWriteResult, PersistError> {
    if wal_is_fatal() {
        return Err(PersistError::Unreconciled {
            snapshot_id: snapshot_id.to_string(),
            stage: PersistStage::Body,
            message: "writer is fatal; refusing a second snapshot id".into(),
        });
    }
    let epoch = require_epoch(lease, snapshot_id)?;
    let (bucket, body_key, commit_key) =
        snapshot_pair_keys(prefix_url, snapshot_id).map_err(|err| PersistError::Definite {
            snapshot_id: snapshot_id.to_string(),
            stage: PersistStage::Body,
            message: err.to_string(),
        })?;
    persist_snapshot_keys(
        store,
        &bucket,
        &body_key,
        &commit_key,
        snapshot_id,
        offsets,
        batches,
        partitions_meta,
        part_meta_blobs,
        lease,
        epoch,
    )
    .await
}

pub async fn persist_snapshot_keys(
    store: &dyn WalObjectStore,
    bucket: &str,
    body_key: &str,
    commit_key: &str,
    snapshot_id: &str,
    offsets: &HashMap<OffsetKey, u64>,
    batches: &HashMap<PartitionKey, Vec<RecordBatch>>,
    partitions_meta: &HashMap<PartitionKey, (u64, SystemTime)>,
    part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
    lease: &LeaseGuard,
    epoch: skippr_lease::LeaseEpoch,
) -> Result<SegmentWriteResult, PersistError> {
    if wal_is_fatal() {
        return Err(PersistError::Unreconciled {
            snapshot_id: snapshot_id.to_string(),
            stage: PersistStage::Body,
            message: "writer is fatal; refusing a second snapshot id".into(),
        });
    }
    require_same_epoch(lease, snapshot_id, epoch)?;
    let encoded =
        encode_snapshot(offsets, batches, partitions_meta, part_meta_blobs).map_err(|err| {
            PersistError::Definite {
                snapshot_id: snapshot_id.to_string(),
                stage: PersistStage::Body,
                message: err.to_string(),
            }
        })?;
    let (body_bytes, body_reconciled) = put_or_reconcile(
        store,
        body_key,
        encoded.bytes.clone(),
        snapshot_id,
        PersistStage::Body,
    )
    .await?;
    require_same_epoch(lease, snapshot_id, epoch)?;
    let commit_bytes = SegmentFile::build_commit_header_bytes(
        encoded.meta.num_partitions,
        encoded.meta.total_bytes,
        &encoded.sha256,
    )
    .to_vec();
    let (commit_bytes, commit_reconciled) = put_or_reconcile(
        store,
        commit_key,
        commit_bytes,
        snapshot_id,
        PersistStage::Commit,
    )
    .await?;
    require_same_epoch(lease, snapshot_id, epoch)?;
    let meta = if body_reconciled || commit_reconciled {
        admit_pair_from_store(store, body_key, commit_key, snapshot_id).await?
    } else {
        SegmentFile::admit_owned_pair_bytes(&body_bytes, &commit_bytes).map_err(|err| {
            PersistError::Unreconciled {
                snapshot_id: snapshot_id.to_string(),
                stage: PersistStage::Commit,
                message: format!("bound pair failed after publication: {err}"),
            }
        })?
    };
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

pub fn persist_error_from_io(err: io::Error, snapshot_id: &str) -> io::Error {
    error!(
        "s3 wal persist failed id={} err={} fatal={}",
        snapshot_id,
        err,
        wal_is_fatal()
    );
    err
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::wal_object_store::{ScriptedGet, ScriptedObjectStore, ScriptedPut};
    use arrow::array::Int32Array;
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
        HashMap<PartitionKey, Vec<RecordBatch>>,
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
        batches.insert(key.clone(), vec![batch()]);
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
            &format!("{id}.seg.commit"),
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
    async fn applied_body_and_commit_acks_same_id() {
        reset_wal_fatal();
        let store = ScriptedObjectStore::new();
        let result = persist(&store, "snap-ok", None).await.unwrap();
        assert!(store.inner().contains("snap-ok.seg"));
        assert!(store.inner().contains("snap-ok.seg.commit"));
        assert_eq!(result.location_key(), "snap-ok.seg");
        assert!(!wal_is_fatal());
    }

    #[tokio::test]
    #[serial]
    async fn lost_commit_response_reconciles_same_id() {
        reset_wal_fatal();
        let store = ScriptedObjectStore::new();
        store.script_put("snap-lost.seg.commit", vec![ScriptedPut::ApplyLostResponse]);
        let result = persist(&store, "snap-lost", None).await.unwrap();
        assert_eq!(result.location_key(), "snap-lost.seg");
        assert!(store.inner().contains("snap-lost.seg.commit"));
        assert!(!wal_is_fatal());
    }

    #[tokio::test]
    #[serial]
    async fn lost_body_response_reconciles_same_id() {
        reset_wal_fatal();
        let store = ScriptedObjectStore::new();
        store.script_put("snap-lost-body.seg", vec![ScriptedPut::ApplyLostResponse]);
        let result = persist(&store, "snap-lost-body", None).await.unwrap();
        assert_eq!(result.location_key(), "snap-lost-body.seg");
        assert!(store.inner().contains("snap-lost-body.seg"));
        assert!(store.inner().contains("snap-lost-body.seg.commit"));
        assert!(!wal_is_fatal());
    }

    #[tokio::test]
    #[serial]
    async fn never_landed_body_is_definite_and_does_not_restore() {
        reset_wal_fatal();
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
        assert!(!wal_is_fatal());
    }

    #[tokio::test]
    #[serial]
    async fn get_failure_after_unknown_body_put_is_fatal_and_blocks_second_id() {
        reset_wal_fatal();
        let store = ScriptedObjectStore::new();
        store.script_put("snap-unk-body.seg", vec![ScriptedPut::ApplyLostResponse]);
        store.script_get("snap-unk-body.seg", ScriptedGet::AlwaysFail);
        let err = persist_err(&store, "snap-unk-body", None).await;
        assert!(matches!(err, PersistError::Unreconciled { .. }));
        assert!(wal_is_fatal());
        let second = persist_err(&store, "snap-other-body", None).await;
        assert_eq!(second.snapshot_id(), "snap-other-body");
        assert!(matches!(second, PersistError::Unreconciled { .. }));
        assert!(!store.inner().contains("snap-other-body.seg"));
    }

    #[tokio::test]
    #[serial]
    async fn lost_commit_admits_from_store_pair() {
        reset_wal_fatal();
        let store = ScriptedObjectStore::new();
        store.script_put("snap-pair.seg.commit", vec![ScriptedPut::ApplyLostResponse]);
        store.script_get("snap-pair.seg", ScriptedGet::FailOnceThenLive);
        let result = persist(&store, "snap-pair", None).await.unwrap();
        assert_eq!(result.location_key(), "snap-pair.seg");
        assert!(store.inner().contains("snap-pair.seg.commit"));
        assert!(!wal_is_fatal());
    }

    #[tokio::test]
    #[serial]
    async fn never_landed_commit_is_definite_and_does_not_restore() {
        reset_wal_fatal();
        let store = ScriptedObjectStore::new();
        store.script_put("snap-miss.seg.commit", vec![ScriptedPut::NeverLand]);
        let err = persist_err(&store, "snap-miss", None).await;
        assert!(matches!(
            err,
            PersistError::Definite {
                stage: PersistStage::Commit,
                ..
            }
        ));
        assert_eq!(err.snapshot_id(), "snap-miss");
        assert!(!store.inner().contains("snap-miss.seg.commit"));
        assert!(!wal_is_fatal());
    }

    #[tokio::test]
    #[serial]
    async fn pair_admit_missing_companion_is_definite_and_allows_retry() {
        reset_wal_fatal();
        let store = ScriptedObjectStore::new();
        store.script_put("snap-gone.seg", vec![ScriptedPut::ApplyLostResponse]);
        store.script_get("snap-gone.seg", ScriptedGet::FoundThenMissing);
        let err = persist_err(&store, "snap-gone", None).await;
        assert!(matches!(
            err,
            PersistError::Definite {
                stage: PersistStage::Body,
                ..
            }
        ));
        assert!(!wal_is_fatal());
        let second = persist(&store, "snap-gone-retry", None).await.unwrap();
        assert_eq!(second.location_key(), "snap-gone-retry.seg");
        assert!(!wal_is_fatal());
    }

    #[tokio::test]
    #[serial]
    async fn get_failure_after_unknown_put_is_fatal_and_blocks_second_id() {
        reset_wal_fatal();
        let store = ScriptedObjectStore::new();
        store.script_put("snap-unk.seg.commit", vec![ScriptedPut::ApplyLostResponse]);
        store.script_get("snap-unk.seg.commit", ScriptedGet::AlwaysFail);
        let err = persist_err(&store, "snap-unk", None).await;
        assert!(matches!(err, PersistError::Unreconciled { .. }));
        assert!(wal_is_fatal());
        let second = persist_err(&store, "snap-other", None).await;
        assert_eq!(second.snapshot_id(), "snap-other");
        assert!(matches!(second, PersistError::Unreconciled { .. }));
        assert!(!store.inner().contains("snap-other.seg"));
    }

    #[tokio::test]
    #[serial]
    async fn get_retry_then_admit_same_id() {
        reset_wal_fatal();
        let store = ScriptedObjectStore::new();
        store.script_put(
            "snap-retry.seg.commit",
            vec![ScriptedPut::ApplyLostResponse],
        );
        store.script_get("snap-retry.seg.commit", ScriptedGet::FailOnceThenLive);
        let result = persist(&store, "snap-retry", None).await.unwrap();
        assert_eq!(result.location_key(), "snap-retry.seg");
        assert!(!wal_is_fatal());
    }

    #[tokio::test]
    #[serial]
    async fn stale_epoch_cannot_ack() {
        reset_wal_fatal();
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
        assert!(object_store.contains("reclaim_owned_pair"));
        assert!(persist.contains("lease: &LeaseGuard"));
        assert!(object_store.contains("lease: &skippr_lease::LeaseGuard"));
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
