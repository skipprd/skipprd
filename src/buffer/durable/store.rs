use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use skippr_lease::{
    CommitIndex, DurableError, LeaseGuard, OffsetPublishError, PipelineKey, PipelinePaths,
    SegmentId,
};
use tokio::sync::Mutex;

use super::apply::DurableApplicator;
use super::log::MutationLog;
use super::mutation::{
    CommittedCheckpoint, CommittedOffset, DurableMutation, MutationEnvelope, SegmentDescriptor,
};
use super::replicate::ReplicationMode;
use crate::buffer::compaction_transaction::CompactionTransaction;
use crate::buffer::segment_file::{PartitionKey, SegmentFile};
use crate::buffer::wal_store::{SegmentWriteLocation, SegmentWriteResult, WalStore};
use crate::helpers::offsets::{OffsetKey, Offsets};

pub enum OffsetMode {
    Sled(Arc<Offsets>),
    Dynamo(Arc<dyn ClusterOffsetPublisher>),
}

#[async_trait]
pub trait ClusterOffsetPublisher: Send + Sync {
    async fn publish(&self, publication: &OffsetPublication) -> Result<(), OffsetPublishError>;
}

#[derive(Clone, Debug)]
pub struct OffsetPublication {
    pub token: CommitToken,
    pub offsets: Vec<CommittedOffset>,
    pub checkpoints: Vec<CommittedCheckpoint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitToken {
    pub epoch: skippr_lease::LeaseEpoch,
    pub index: CommitIndex,
}

pub struct PipelineDurableStore {
    key: PipelineKey,
    paths: PipelinePaths,
    guard: Arc<LeaseGuard>,
    log: Mutex<MutationLog>,
    applicator: DurableApplicator,
    replicator: ReplicationMode,
    offsets: OffsetMode,
    commit_lock: Mutex<()>,
}

static ACTIVE: OnceLock<RwLock<HashMap<PipelineKey, Arc<PipelineDurableStore>>>> = OnceLock::new();

fn active_slot() -> &'static RwLock<HashMap<PipelineKey, Arc<PipelineDurableStore>>> {
    ACTIVE.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn durable_store_for(key: &PipelineKey) -> Option<Arc<PipelineDurableStore>> {
    active_slot()
        .read()
        .ok()
        .and_then(|map| map.get(key).cloned())
}

pub fn all_durable_stores() -> Vec<Arc<PipelineDurableStore>> {
    active_slot()
        .read()
        .ok()
        .map(|map| map.values().cloned().collect())
        .unwrap_or_default()
}

pub fn install_durable_store(store: Arc<PipelineDurableStore>) {
    if let Ok(mut map) = active_slot().write() {
        map.insert(store.key.clone(), store);
    }
}

pub fn remove_durable_store(key: &PipelineKey) {
    if let Ok(mut map) = active_slot().write() {
        map.remove(key);
    }
}

pub fn clear_active_durable_store() {
    if let Ok(mut map) = active_slot().write() {
        map.clear();
    }
}

impl PipelineDurableStore {
    pub fn new(
        key: PipelineKey,
        paths: PipelinePaths,
        guard: Arc<LeaseGuard>,
        log: MutationLog,
        replicator: ReplicationMode,
        offsets: OffsetMode,
    ) -> Arc<Self> {
        let applicator = DurableApplicator::new(paths.clone());
        match (&replicator, &offsets) {
            (ReplicationMode::Synchronous(_), OffsetMode::Sled(_)) => {
                panic!("clustered WAL requires DynamoDB offsets; sled is disk-mode only");
            }
            _ => {}
        }
        Arc::new(Self {
            key,
            paths,
            guard,
            log: Mutex::new(log),
            applicator,
            replicator,
            offsets,
            commit_lock: Mutex::new(()),
        })
    }

    pub fn key(&self) -> &PipelineKey {
        &self.key
    }

    pub fn paths(&self) -> &PipelinePaths {
        &self.paths
    }

    pub async fn durable_state(&self) -> crate::buffer::durable::log::DurableState {
        self.log.lock().await.state()
    }

    pub async fn committed_envelopes(&self) -> Vec<MutationEnvelope> {
        self.log.lock().await.committed_envelopes().to_vec()
    }

    pub async fn committed_envelopes_in_range(
        &self,
        from: u64,
        to: u64,
        limit: usize,
    ) -> Vec<MutationEnvelope> {
        self.log
            .lock()
            .await
            .envelopes_in_range(from, to)
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn guard(&self) -> &Arc<LeaseGuard> {
        &self.guard
    }

    pub async fn recover_unknown_prepared(
        &self,
        identity: &crate::cluster::identity::ClusterIdentity,
        peers: &[SocketAddr],
    ) -> Result<(), DurableError> {
        let mut log = self.log.lock().await;
        recover_unproven_prepared(&mut log, &self.applicator, &self.key, identity, peers).await
    }

    pub async fn replay_unapplied(&self) -> Result<(), DurableError> {
        let envelopes = {
            let log = self.log.lock().await;
            log.committed_envelopes()
                .iter()
                .filter(|envelope| envelope.index > log.applied_index())
                .cloned()
                .collect::<Vec<_>>()
        };
        for envelope in envelopes {
            self.applicator.apply(&envelope)?;
            let mut log = self.log.lock().await;
            log.mark_applied(envelope.index, envelope.entry_hash()?)?;
        }
        Ok(())
    }

    pub async fn compaction_sot(&self) -> Result<super::snapshot::StateSnapshot, DurableError> {
        let log = self.log.lock().await;
        super::snapshot::clustered_compaction_sot(&self.paths, &log, &self.key)
    }

    pub async fn commit_put_compaction(
        &self,
        transaction: CompactionTransaction,
    ) -> Result<(), DurableError> {
        self.commit_mutation(DurableMutation::PutCompaction { transaction }, [0u8; 32])
            .await
    }

    pub async fn commit_complete_slices(
        &self,
        compaction_id: String,
        entries: Vec<super::mutation::CompletedOrdinals>,
    ) -> Result<(), DurableError> {
        self.commit_mutation(
            DurableMutation::CompleteSlices {
                compaction_id,
                entries,
            },
            [0u8; 32],
        )
        .await
    }

    pub async fn commit_reclaim_segment(&self, segment_id: String) -> Result<(), DurableError> {
        self.commit_mutation(DurableMutation::ReclaimSegment { segment_id }, [0u8; 32])
            .await
    }

    async fn finish_pending_locked(&self, log: &mut MutationLog) -> Result<(), DurableError> {
        let pending = log.pending_prepared().to_vec();
        for envelope in pending {
            let payload = match &envelope.body {
                DurableMutation::CommitSegment { descriptor, .. } => {
                    let id = SegmentId::new(&descriptor.segment_id)
                        .map_err(|err| DurableError::Io(err.to_string()))?;
                    self.paths.segment(&id)
                }
                _ => PathBuf::new(),
            };
            replicate_prepared(
                log,
                &self.replicator,
                &envelope,
                &payload,
                &self.guard,
                &self.paths.root,
            )
            .await?;
            let hash = envelope.entry_hash()?;
            log.append_committed(envelope.index, hash)?;
            self.applicator.apply(&envelope)?;
            log.mark_applied(envelope.index, hash)?;
            if matches!(envelope.body, DurableMutation::ReclaimSegment { .. }) {
                let snapshot = super::snapshot::retain_live_snapshot(log, &self.paths, &self.key)?;
                crate::buffer::ingest_buffer::Buffers::sync_planner_from_snapshot(&snapshot);
            }
        }
        Ok(())
    }

    async fn commit_mutation(
        &self,
        body: DurableMutation,
        payload_sha256: [u8; 32],
    ) -> Result<(), DurableError> {
        let _serial = self.commit_lock.lock().await;
        let epoch = self.guard.require_active_epoch()?;
        let mut log = self.log.lock().await;
        self.finish_pending_locked(&mut log).await?;
        let next = log
            .committed_index()
            .next()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let envelope = MutationEnvelope {
            protocol_version: skippr_lease::CURRENT_PROTOCOL,
            pipeline: self.key.clone(),
            epoch,
            index: next,
            previous_hash: log.head_hash(),
            payload_sha256,
            body,
        };
        log.append_prepared(&envelope)?;
        hit_prepared_window(&mut log, &self.paths.root)?;
        if let Err(err) = replicate_prepared(
            &mut log,
            &self.replicator,
            &envelope,
            Path::new(""),
            &self.guard,
            &self.paths.root,
        )
        .await
        {
            return Err(err);
        }
        self.guard.require_same_active_epoch(epoch)?;
        let hash = envelope.entry_hash()?;
        log.append_committed(next, hash)?;
        self.applicator.apply(&envelope)?;
        log.mark_applied(next, hash)?;
        if matches!(envelope.body, DurableMutation::ReclaimSegment { .. }) {
            let snapshot = super::snapshot::retain_live_snapshot(&mut log, &self.paths, &self.key)?;
            crate::buffer::ingest_buffer::Buffers::sync_planner_from_snapshot(&snapshot);
        }
        if !log.has_format_marker() {
            log.write_format_marker()?;
        }
        Ok(())
    }

    async fn adopt_committed_segment(
        &self,
        log: &mut MutationLog,
        epoch: skippr_lease::LeaseEpoch,
        segment_id: &str,
    ) -> Result<SegmentWriteResult, DurableError> {
        let envelope = log
            .committed_envelopes()
            .iter()
            .rev()
            .find(|envelope| {
                matches!(
                    &envelope.body,
                    DurableMutation::CommitSegment { descriptor, .. }
                        if descriptor.segment_id == segment_id
                )
            })
            .cloned()
            .ok_or_else(|| {
                DurableError::Io(format!(
                    "pending commit segment {segment_id} missing after finish"
                ))
            })?;
        let DurableMutation::CommitSegment {
            descriptor,
            offsets,
            checkpoints,
        } = envelope.body
        else {
            return Err(DurableError::Io(format!(
                "pending commit segment {segment_id} is not a CommitSegment"
            )));
        };
        let id = SegmentId::new(&descriptor.segment_id)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let seg_file = SegmentFile {
            path: self.paths.segment(&id),
        };
        let meta = seg_file.read_metadata()?;
        if !log.has_format_marker() {
            log.write_format_marker()?;
        }
        self.publish_commit_offsets(log, epoch, envelope.index, offsets, checkpoints)
            .await?;
        let location = match &self.replicator {
            ReplicationMode::LocalOnly => SegmentWriteLocation::Disk {
                path: seg_file.path.clone(),
            },
            ReplicationMode::Synchronous(_) => SegmentWriteLocation::Clustered {
                path: seg_file.path.clone(),
                uri: self.paths.wal_uri(&self.key, &id),
            },
        };
        Ok(SegmentWriteResult {
            meta,
            total_rows: 0,
            sha256: descriptor.payload_sha256,
            location,
            offsets_published: true,
        })
    }

    async fn publish_commit_offsets(
        &self,
        log: &mut MutationLog,
        epoch: skippr_lease::LeaseEpoch,
        index: CommitIndex,
        committed_offsets: Vec<CommittedOffset>,
        checkpoints: Vec<CommittedCheckpoint>,
    ) -> Result<(), DurableError> {
        crate::cluster::failpoint::hit(
            crate::cluster::failpoint::FailpointName::AfterLocalCommit,
            &self.paths.root,
        )?;
        match &self.offsets {
            OffsetMode::Sled(offsets_db) => {
                publish_sled(offsets_db, &committed_offsets, &checkpoints)?;
            }
            OffsetMode::Dynamo(publisher) => {
                publish_offsets_until_success_or_fenced(
                    publisher.as_ref(),
                    &OffsetPublication {
                        token: CommitToken { epoch, index },
                        offsets: committed_offsets,
                        checkpoints,
                    },
                    &self.guard,
                )
                .await?;
            }
        }
        log.mark_offsets_published(index)?;
        crate::cluster::failpoint::hit(
            crate::cluster::failpoint::FailpointName::AfterOffsetsPublished,
            &self.paths.root,
        )?;
        Ok(())
    }

    pub async fn commit_segment(
        &self,
        snapshot_id: &str,
        offsets: &HashMap<OffsetKey, u64>,
        batches: &HashMap<PartitionKey, Vec<arrow::array::RecordBatch>>,
        partitions_meta: &HashMap<PartitionKey, (u64, SystemTime)>,
        part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
        checkpoint_updates: &HashMap<String, crate::plugins::cdc::CheckpointEnvelope>,
    ) -> Result<SegmentWriteResult, DurableError> {
        let _serial = self.commit_lock.lock().await;
        let epoch = self.guard.require_active_epoch()?;
        let mut log = self.log.lock().await;
        let retry_segment_id = matching_pending_commit_segment(log.pending_prepared(), offsets);
        self.finish_pending_locked(&mut log).await?;
        if let Some(segment_id) = retry_segment_id {
            return self
                .adopt_committed_segment(&mut log, epoch, &segment_id)
                .await;
        }
        let next = log
            .committed_index()
            .next()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let id = SegmentId::new(snapshot_id).map_err(|err| DurableError::Io(err.to_string()))?;
        fs_err_create(&self.paths.segs)?;
        let seg_file = SegmentFile::new(&self.paths.segs, snapshot_id)?;
        let (meta, total_rows, sha256) =
            seg_file.write_snapshot(offsets, batches, partitions_meta, part_meta_blobs)?;
        let mut committed_offsets: Vec<CommittedOffset> = Vec::with_capacity(offsets.len());
        for (key, position) in offsets {
            let existing = match &self.offsets {
                OffsetMode::Sled(db) => db
                    .snapshot_value(key)
                    .map_err(|err| DurableError::Io(err.to_string()))?,
                OffsetMode::Dynamo(_) => None,
            };
            // Clustered persist never calls `mark_offsets_durable_in_wal`.
            // These tuples are the DynamoDB publication payload (ingest-offsets.md).
            committed_offsets.push(CommittedOffset::for_wal_commit(
                key,
                *position,
                existing.as_ref(),
            ));
        }
        committed_offsets
            .sort_by(|a, b| (&a.namespace, &a.partition).cmp(&(&b.namespace, &b.partition)));
        let mut checkpoints = Vec::with_capacity(checkpoint_updates.len());
        for (key, envelope) in checkpoint_updates {
            checkpoints
                .push(CommittedCheckpoint::from_envelope(key, envelope).map_err(DurableError::Io)?);
        }
        checkpoints.sort_by(|a, b| a.logical_key.cmp(&b.logical_key));
        let mut schema_fingerprints: Vec<String> = meta
            .index
            .iter()
            .map(|idx| idx.key.schema_fingerprint.clone())
            .filter(|fp| !fp.is_empty())
            .collect();
        schema_fingerprints.sort();
        schema_fingerprints.dedup();
        let descriptor = SegmentDescriptor {
            segment_id: snapshot_id.to_string(),
            payload_len: meta.total_bytes,
            payload_sha256: sha256,
            num_partitions: meta.num_partitions,
            total_bytes: meta.total_bytes,
            created_at_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            schema_fingerprints,
        };
        let envelope = MutationEnvelope {
            protocol_version: skippr_lease::CURRENT_PROTOCOL,
            pipeline: self.key.clone(),
            epoch,
            index: next,
            previous_hash: log.head_hash(),
            payload_sha256: sha256,
            body: DurableMutation::CommitSegment {
                descriptor: descriptor.clone(),
                offsets: committed_offsets.clone(),
                checkpoints: checkpoints.clone(),
            },
        };
        log.append_prepared(&envelope)?;
        hit_prepared_window(&mut log, &self.paths.root)?;
        if let Err(err) = replicate_prepared(
            &mut log,
            &self.replicator,
            &envelope,
            &self.paths.segment(&id),
            &self.guard,
            &self.paths.root,
        )
        .await
        {
            return Err(err);
        }
        self.guard.require_same_active_epoch(epoch)?;
        let hash = envelope.entry_hash()?;
        log.append_committed(next, hash)?;
        self.applicator.apply(&envelope)?;
        log.mark_applied(next, hash)?;
        if !log.has_format_marker() {
            log.write_format_marker()?;
        }
        self.publish_commit_offsets(&mut log, epoch, next, committed_offsets, checkpoints)
            .await?;
        let location = match &self.replicator {
            ReplicationMode::LocalOnly => SegmentWriteLocation::Disk {
                path: seg_file.path.clone(),
            },
            ReplicationMode::Synchronous(_) => SegmentWriteLocation::Clustered {
                path: seg_file.path.clone(),
                uri: self.paths.wal_uri(&self.key, &id),
            },
        };
        Ok(SegmentWriteResult {
            meta,
            total_rows,
            sha256,
            location,
            offsets_published: true,
        })
    }

    pub async fn reconcile_published_offsets(&self) -> Result<(), DurableError> {
        let snapshot_offsets = super::snapshot::read_snapshot(&self.paths)?
            .map(|snap| (snap.offsets, snap.checkpoints))
            .unwrap_or_default();
        let envelopes = {
            let log = self.log.lock().await;
            log.committed_envelopes().to_vec()
        };
        let mut offsets = snapshot_offsets.0;
        let mut checkpoints = snapshot_offsets.1;
        let mut last_index = skippr_lease::CommitIndex::ZERO;
        for envelope in envelopes {
            if let DurableMutation::CommitSegment {
                offsets: next_offsets,
                checkpoints: next_checkpoints,
                ..
            } = envelope.body
            {
                offsets = next_offsets;
                checkpoints = next_checkpoints;
                last_index = envelope.index;
            }
        }
        let token = CommitToken {
            epoch: self.guard.require_offset_epoch()?,
            index: last_index,
        };
        match &self.offsets {
            OffsetMode::Sled(offsets_db) => publish_sled(offsets_db, &offsets, &checkpoints)?,
            OffsetMode::Dynamo(publisher) => {
                publish_offsets_until_success_or_fenced(
                    publisher.as_ref(),
                    &OffsetPublication {
                        token,
                        offsets,
                        checkpoints,
                    },
                    &self.guard,
                )
                .await?;
            }
        }
        Ok(())
    }
}

pub(crate) async fn recover_unproven_prepared(
    log: &mut MutationLog,
    applicator: &DurableApplicator,
    key: &PipelineKey,
    identity: &crate::cluster::identity::ClusterIdentity,
    peers: &[SocketAddr],
) -> Result<(), DurableError> {
    let pending = log.pending_prepared().to_vec();
    for envelope in pending {
        let mut found = false;
        for endpoint in peers {
            if let Ok(status) = crate::cluster::peer::query_status(*endpoint, key, identity).await {
                if status.committed_index == envelope.index.get()
                    && status.head_hash == envelope.entry_hash()?.to_vec()
                {
                    found = true;
                    break;
                }
                if let Ok(entries) = crate::cluster::peer::fetch_entries_from(
                    *endpoint,
                    key,
                    envelope.index.get(),
                    envelope.index.get(),
                    identity,
                )
                .await
                {
                    if entries
                        .iter()
                        .any(|peer| peer.envelope.entry_hash().ok() == envelope.entry_hash().ok())
                    {
                        found = true;
                        break;
                    }
                }
            }
        }
        if found {
            let hash = envelope.entry_hash()?;
            log.append_committed(envelope.index, hash)?;
            applicator.apply(&envelope)?;
            log.mark_applied(envelope.index, hash)?;
        } else {
            tracing::warn!(
                pipeline = %key.pipeline(),
                index = envelope.index.get(),
                "preserving prepared entry with no committed peer copy"
            );
            return Err(DurableError::UnprovenPrepared(envelope.index.get()));
        }
    }
    Ok(())
}

fn hit_prepared_window(log: &mut MutationLog, root: &Path) -> Result<(), DurableError> {
    if let Err(err) = crate::cluster::failpoint::hit(
        crate::cluster::failpoint::FailpointName::PreparedDiskFull,
        root,
    ) {
        log.abort_pending_prepared()?;
        return Err(err);
    }
    crate::cluster::failpoint::hit(crate::cluster::failpoint::FailpointName::PreparedIo, root)?;
    crate::cluster::failpoint::hit(
        crate::cluster::failpoint::FailpointName::AfterPrepared,
        root,
    )?;
    Ok(())
}

fn matching_pending_commit_segment(
    pending: &[MutationEnvelope],
    offsets: &HashMap<OffsetKey, u64>,
) -> Option<String> {
    let envelope = pending.first()?;
    let DurableMutation::CommitSegment {
        descriptor,
        offsets: committed,
        ..
    } = &envelope.body
    else {
        return None;
    };
    if committed.len() != offsets.len() {
        return None;
    }
    let same = offsets.iter().all(|(key, position)| {
        committed.iter().any(|offset| {
            offset.namespace == key.namespace
                && offset.partition == key.partition
                && offset.position == *position
        })
    });
    same.then(|| descriptor.segment_id.clone())
}

fn replica_reject_is_definite(err: &DurableError) -> bool {
    matches!(
        err,
        DurableError::StaleEpoch
            | DurableError::NotCaughtUp
            | DurableError::DiskFull
            | DurableError::Diverged(_)
    )
}

async fn replicate_prepared(
    log: &mut MutationLog,
    replicator: &ReplicationMode,
    envelope: &MutationEnvelope,
    payload: &Path,
    guard: &LeaseGuard,
    root: &Path,
) -> Result<(), DurableError> {
    let ReplicationMode::Synchronous(replica) = replicator else {
        return Ok(());
    };
    match replica
        .commit_at(replica.snapshot_endpoint(), envelope, payload, guard)
        .await
    {
        Ok(()) => crate::cluster::failpoint::hit(
            crate::cluster::failpoint::FailpointName::AfterReplicaAck,
            root,
        ),
        Err(err) if replica_reject_is_definite(&err) => {
            log.abort_pending_prepared()?;
            Err(err)
        }
        Err(err) => {
            // Keep Prepared. Do not fence the primary session: replace_task
            // must assign a live replica and the next commit retries this envelope.
            Err(err)
        }
    }
}

fn publish_sled(
    offsets: &Offsets,
    committed: &[CommittedOffset],
    checkpoints: &[CommittedCheckpoint],
) -> Result<(), DurableError> {
    for offset in committed {
        let key = OffsetKey::new(&offset.namespace, &offset.partition);
        offsets.set(
            &key,
            crate::helpers::offsets::OffsetTypes::Closed,
            offset.closed,
        );
        offsets.set(
            &key,
            crate::helpers::offsets::OffsetTypes::Position,
            offset.position,
        );
    }
    let _ = checkpoints;
    Ok(())
}

fn fs_err_create(path: &std::path::Path) -> Result<(), DurableError> {
    std::fs::create_dir_all(path).map_err(DurableError::from)
}

const OFFSET_RETRY_MIN: Duration = Duration::from_millis(50);
const OFFSET_RETRY_MAX: Duration = Duration::from_secs(2);

async fn publish_offsets_until_success_or_fenced(
    publisher: &dyn ClusterOffsetPublisher,
    publication: &OffsetPublication,
    guard: &LeaseGuard,
) -> Result<(), DurableError> {
    let mut delay = OFFSET_RETRY_MIN;
    loop {
        if guard
            .require_same_offset_epoch(publication.token.epoch)
            .is_err()
        {
            return Err(DurableError::FencedAfterCommit);
        }
        match guard.run_until_fenced(publisher.publish(publication)).await {
            Err(_) => return Err(DurableError::FencedAfterCommit),
            Ok(Ok(())) => return Ok(()),
            Ok(Err(OffsetPublishError::Corrupt(err))) => {
                return Err(DurableError::CorruptOffset(err));
            }
            Ok(Err(OffsetPublishError::Transient(_))) => {
                if guard.sleep_or_fence(delay).await.is_err() {
                    return Err(DurableError::FencedAfterCommit);
                }
                delay = (delay * 2).min(OFFSET_RETRY_MAX);
            }
        }
    }
}

pub struct MemoryOffsetPublisher {
    pub published: Mutex<Vec<OffsetPublication>>,
    fail_remaining: std::sync::atomic::AtomicUsize,
}

impl MemoryOffsetPublisher {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            published: Mutex::new(Vec::new()),
            fail_remaining: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn fail_next(&self, n: usize) {
        self.fail_remaining
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl ClusterOffsetPublisher for MemoryOffsetPublisher {
    async fn publish(&self, publication: &OffsetPublication) -> Result<(), OffsetPublishError> {
        if self
            .fail_remaining
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_ok()
        {
            return Err(OffsetPublishError::Transient("injected".into()));
        }
        self.published.lock().await.push(publication.clone());
        Ok(())
    }
}

#[cfg(feature = "offset-store-dynamodb")]
pub struct DynamoOffsetPublisher {
    store: Arc<skippr_offset_store_dynamodb::DynamoDbOffsetStore>,
}

#[cfg(feature = "offset-store-dynamodb")]
impl DynamoOffsetPublisher {
    pub fn new(store: Arc<skippr_offset_store_dynamodb::DynamoDbOffsetStore>) -> Arc<Self> {
        Arc::new(Self { store })
    }
}

#[cfg(feature = "offset-store-dynamodb")]
#[async_trait]
impl ClusterOffsetPublisher for DynamoOffsetPublisher {
    async fn publish(&self, publication: &OffsetPublication) -> Result<(), OffsetPublishError> {
        for offset in &publication.offsets {
            let sk = format!("offset#{}#{}", offset.namespace, offset.partition);
            let mut bytes = Vec::with_capacity(24);
            bytes.extend_from_slice(&offset.filesize.to_le_bytes());
            bytes.extend_from_slice(&offset.position.to_le_bytes());
            bytes.extend_from_slice(&offset.closed.to_le_bytes());
            let sha = hex::encode({
                use sha2::{Digest, Sha256};
                Sha256::digest(&bytes)
            });
            self.store
                .publish_wal_commit(
                    &sk,
                    &bytes,
                    publication.token.epoch.get(),
                    publication.token.index.get(),
                    &sha,
                )
                .map_err(|err| {
                    if err.contains("corrupt") {
                        OffsetPublishError::Corrupt(err)
                    } else {
                        OffsetPublishError::Transient(err)
                    }
                })?;
        }
        for checkpoint in &publication.checkpoints {
            let sk = format!("checkpoint#{}", checkpoint.logical_key);
            let sha = hex::encode({
                use sha2::{Digest, Sha256};
                Sha256::digest(&checkpoint.envelope)
            });
            self.store
                .publish_wal_commit(
                    &sk,
                    &checkpoint.envelope,
                    publication.token.epoch.get(),
                    publication.token.index.get(),
                    &sha,
                )
                .map_err(OffsetPublishError::Transient)?;
        }
        Ok(())
    }
}

#[cfg(feature = "offset-store-cloud-tables")]
pub struct CloudOffsetPublisher {
    store: Arc<skippr_store_cloud_tables::CloudTablesOffsetStore>,
}

#[cfg(feature = "offset-store-cloud-tables")]
impl CloudOffsetPublisher {
    pub fn new(store: Arc<skippr_store_cloud_tables::CloudTablesOffsetStore>) -> Arc<Self> {
        Arc::new(Self { store })
    }
}

#[cfg(feature = "offset-store-cloud-tables")]
#[async_trait]
impl ClusterOffsetPublisher for CloudOffsetPublisher {
    async fn publish(&self, publication: &OffsetPublication) -> Result<(), OffsetPublishError> {
        for offset in &publication.offsets {
            let sk = format!("offset#{}#{}", offset.namespace, offset.partition);
            let mut bytes = Vec::with_capacity(24);
            bytes.extend_from_slice(&offset.filesize.to_le_bytes());
            bytes.extend_from_slice(&offset.position.to_le_bytes());
            bytes.extend_from_slice(&offset.closed.to_le_bytes());
            let sha = hex::encode({
                use sha2::{Digest, Sha256};
                Sha256::digest(&bytes)
            });
            self.store
                .publish_wal_commit(
                    &sk,
                    &bytes,
                    publication.token.epoch.get(),
                    publication.token.index.get(),
                    &sha,
                )
                .map_err(|err| {
                    if err.contains("corrupt") {
                        OffsetPublishError::Corrupt(err)
                    } else {
                        OffsetPublishError::Transient(err)
                    }
                })?;
        }
        for checkpoint in &publication.checkpoints {
            let sk = format!("checkpoint#{}", checkpoint.logical_key);
            let sha = hex::encode({
                use sha2::{Digest, Sha256};
                Sha256::digest(&checkpoint.envelope)
            });
            self.store
                .publish_wal_commit(
                    &sk,
                    &checkpoint.envelope,
                    publication.token.epoch.get(),
                    publication.token.index.get(),
                    &sha,
                )
                .map_err(OffsetPublishError::Transient)?;
        }
        Ok(())
    }
}

pub struct ClusteredWalStore;

#[async_trait]
impl WalStore for ClusteredWalStore {
    async fn write_snapshot_and_commit(
        &self,
        snapshot_id: &str,
        offsets: &HashMap<OffsetKey, u64>,
        batches: &HashMap<PartitionKey, Vec<arrow::array::RecordBatch>>,
        partitions_meta: &HashMap<PartitionKey, (u64, SystemTime)>,
        part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
        checkpoint_updates: &HashMap<String, crate::plugins::cdc::CheckpointEnvelope>,
    ) -> std::io::Result<SegmentWriteResult> {
        let store = crate::buffer::wal_store::ingest_durable_store().ok_or_else(|| {
            std::io::Error::other("WAL write requires an ActivePrimary durable store")
        })?;
        store
            .commit_segment(
                snapshot_id,
                offsets,
                batches,
                partitions_meta,
                part_meta_blobs,
                checkpoint_updates,
            )
            .await
            .map_err(|err| std::io::Error::other(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skippr_lease::{LeaseEpoch, LeaseSession, MonoInstant, NodeId, PipelineKey, SystemClock};
    use std::sync::Arc;

    fn leased_guard(key: PipelineKey) -> Arc<LeaseGuard> {
        let session = elect_session();
        let guard = LeaseGuard::owner_elect(key, session.clone(), Arc::new(SystemClock::new()));
        guard.activate(session).unwrap();
        guard
    }

    fn elect_session() -> LeaseSession {
        LeaseSession {
            owner: NodeId::generate(),
            epoch: LeaseEpoch::new(1),
            heartbeat: 1,
            initialized: true,
            local_deadline: MonoInstant::from_nanos(u64::MAX),
        }
    }

    fn owner_elect_guard(key: PipelineKey) -> Arc<LeaseGuard> {
        LeaseGuard::owner_elect(key, elect_session(), Arc::new(SystemClock::new()))
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn local_commit_segment_writes_commit_marker() {
        struct NoopPublisher;
        #[async_trait]
        impl ClusterOffsetPublisher for NoopPublisher {
            async fn publish(
                &self,
                _publication: &OffsetPublication,
            ) -> Result<(), OffsetPublishError> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let log = super::super::log::MutationLog::open(paths.clone()).unwrap();
        let store = PipelineDurableStore::new(
            key,
            paths.clone(),
            guard,
            log,
            ReplicationMode::LocalOnly,
            OffsetMode::Dynamo(Arc::new(NoopPublisher)),
        );
        let result = store
            .commit_segment(
                "seg-test",
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert!(result.offsets_published);
        assert!(matches!(result.location, SegmentWriteLocation::Disk { .. }));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn dynamo_offset_publish_retries_transient_then_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let log = super::super::log::MutationLog::open(paths.clone()).unwrap();
        let publisher = MemoryOffsetPublisher::new();
        publisher.fail_next(1);
        let store = PipelineDurableStore::new(
            key,
            paths,
            guard,
            log,
            ReplicationMode::LocalOnly,
            OffsetMode::Dynamo(publisher.clone()),
        );
        store
            .commit_segment(
                "seg-retry",
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(publisher.published.lock().await.len(), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn commit_segment_publishes_closed_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let log = super::super::log::MutationLog::open(paths.clone()).unwrap();
        let publisher = MemoryOffsetPublisher::new();
        let store = PipelineDurableStore::new(
            key,
            paths,
            guard,
            log,
            ReplicationMode::LocalOnly,
            OffsetMode::Dynamo(publisher.clone()),
        );
        let offset_key = OffsetKey::new("events", "/tmp/batch1.jsonl");
        let mut offsets = HashMap::new();
        offsets.insert(offset_key, 5u64);
        store
            .commit_segment(
                "seg-closed",
                &offsets,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let published = publisher.published.lock().await;
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].offsets.len(), 1);
        assert_eq!(published[0].offsets[0].namespace, "events");
        assert_eq!(published[0].offsets[0].position, 5);
        assert_eq!(
            published[0].offsets[0].closed, 1,
            "clustered WAL publish is the Closed=1 write; a later primary must not re-scan"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn after_offsets_published_failpoint_fires_once() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let log = super::super::log::MutationLog::open(paths.clone()).unwrap();
        let publisher = MemoryOffsetPublisher::new();
        let store = PipelineDurableStore::new(
            key,
            paths.clone(),
            guard,
            log,
            ReplicationMode::LocalOnly,
            OffsetMode::Dynamo(publisher.clone()),
        );
        crate::cluster::failpoint::arm(crate::cluster::failpoint::AFTER_OFFSETS_PUBLISHED);
        let err = match store
            .commit_segment(
                "seg-fp",
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
        {
            Ok(_) => panic!("failpoint should abort after offset publish"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("after_offsets_published"));
        assert_eq!(publisher.published.lock().await.len(), 1);
        crate::cluster::failpoint::disarm();
    }

    #[tokio::test]
    async fn recover_unknown_prepared_without_peers_preserves_record() {
        use crate::buffer::durable::mutation::{DurableMutation, MutationEnvelope};
        use skippr_lease::{CommitIndex, LeaseEpoch, NodeId, GENESIS_HASH};

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let mut log = MutationLog::open(paths.clone()).unwrap();
        let envelope = MutationEnvelope {
            protocol_version: 1,
            pipeline: key.clone(),
            epoch: LeaseEpoch::new(1),
            index: CommitIndex::new(1),
            previous_hash: GENESIS_HASH,
            payload_sha256: [0u8; 32],
            body: DurableMutation::ReclaimSegment {
                segment_id: "prepared".into(),
            },
        };
        log.append_prepared(&envelope).unwrap();
        let guard = LeaseGuard::replica(
            key.clone(),
            LeaseEpoch::new(1),
            Arc::new(SystemClock::new()),
        );
        let store = PipelineDurableStore::new(
            key,
            paths.clone(),
            guard,
            log,
            ReplicationMode::LocalOnly,
            OffsetMode::Dynamo(MemoryOffsetPublisher::new()),
        );
        let identity = crate::cluster::identity::ClusterIdentity::new(
            skippr_lease::ClusterId::new("test-cluster").unwrap(),
            NodeId::generate(),
        );
        let err = store
            .recover_unknown_prepared(&identity, &[])
            .await
            .unwrap_err();
        assert!(matches!(err, DurableError::UnprovenPrepared(1)));
        let reopened = MutationLog::open(paths).unwrap();
        assert_eq!(reopened.pending_prepared().len(), 1);
        assert_eq!(reopened.committed_index(), CommitIndex::ZERO);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn replicate_timeout_preserves_prepared_without_fencing_lease() {
        use crate::buffer::durable::replicate::QuorumReplicator;
        use crate::cluster::peer::ScriptedPeer;
        use skippr_lease::PipelineLifecycle;

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let guard = leased_guard(key.clone());
        let log = MutationLog::open(paths.clone()).unwrap();
        let store = PipelineDurableStore::new(
            key,
            paths.clone(),
            guard,
            log,
            ReplicationMode::Synchronous(QuorumReplicator::new(
                ScriptedPeer::new(vec![Err(DurableError::Timeout)]),
                "127.0.0.1:1".parse().unwrap(),
            )),
            OffsetMode::Dynamo(MemoryOffsetPublisher::new()),
        );
        let err = store.commit_reclaim_segment("s".into()).await.unwrap_err();
        assert!(matches!(err, DurableError::Timeout));
        assert_ne!(store.guard().lifecycle(), PipelineLifecycle::Fenced);
        let reopened = MutationLog::open(paths).unwrap();
        assert_eq!(reopened.pending_prepared().len(), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn replicate_timeout_retries_prepared_on_next_commit() {
        use crate::buffer::durable::replicate::QuorumReplicator;
        use crate::cluster::peer::ScriptedPeer;

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let guard = leased_guard(key.clone());
        let log = MutationLog::open(paths.clone()).unwrap();
        let store = PipelineDurableStore::new(
            key,
            paths.clone(),
            guard,
            log,
            ReplicationMode::Synchronous(QuorumReplicator::new(
                ScriptedPeer::new(vec![Err(DurableError::Timeout), Ok(()), Ok(())]),
                "127.0.0.1:1".parse().unwrap(),
            )),
            OffsetMode::Dynamo(MemoryOffsetPublisher::new()),
        );
        assert!(store.commit_reclaim_segment("s".into()).await.is_err());
        store.commit_reclaim_segment("t".into()).await.unwrap();
        let reopened = MutationLog::open(paths).unwrap();
        assert!(reopened.pending_prepared().is_empty());
        assert_eq!(reopened.committed_index().get(), 2);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn source_retry_after_quorum_timeout_does_not_commit_duplicate_segment() {
        use crate::buffer::durable::mutation::DurableMutation;
        use crate::buffer::durable::replicate::QuorumReplicator;
        use crate::cluster::peer::ScriptedPeer;
        use crate::helpers::offsets::OffsetKey;
        use skippr_lease::SegmentId;

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let guard = leased_guard(key.clone());
        let log = MutationLog::open(paths.clone()).unwrap();
        let publisher = MemoryOffsetPublisher::new();
        let store = PipelineDurableStore::new(
            key,
            paths.clone(),
            guard,
            log,
            ReplicationMode::Synchronous(QuorumReplicator::new(
                ScriptedPeer::new(vec![Err(DurableError::Timeout), Ok(()), Ok(())]),
                "127.0.0.1:1".parse().unwrap(),
            )),
            OffsetMode::Dynamo(publisher.clone()),
        );
        let mut offsets = HashMap::new();
        offsets.insert(OffsetKey::new("ns", "part"), 2);
        let first = store
            .commit_segment(
                "seg-a",
                &offsets,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await;
        assert!(first.is_err());
        let retry = store
            .commit_segment(
                "seg-b",
                &offsets,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert!(retry.offsets_published);
        let reopened = MutationLog::open(paths.clone()).unwrap();
        assert!(reopened.pending_prepared().is_empty());
        assert_eq!(reopened.committed_index().get(), 1);
        let committed: Vec<_> = reopened
            .committed_envelopes()
            .iter()
            .filter_map(|envelope| match &envelope.body {
                DurableMutation::CommitSegment { descriptor, .. } => {
                    Some(descriptor.segment_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(committed, vec!["seg-a"]);
        assert!(paths.segment(&SegmentId::new("seg-a").unwrap()).exists());
        assert!(!paths.segment(&SegmentId::new("seg-b").unwrap()).exists());
        assert_eq!(publisher.published.lock().await.len(), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn source_retry_with_new_offsets_still_commits_next_segment() {
        use crate::buffer::durable::replicate::QuorumReplicator;
        use crate::cluster::peer::ScriptedPeer;
        use crate::helpers::offsets::OffsetKey;

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let guard = leased_guard(key.clone());
        let log = MutationLog::open(paths.clone()).unwrap();
        let store = PipelineDurableStore::new(
            key,
            paths.clone(),
            guard,
            log,
            ReplicationMode::Synchronous(QuorumReplicator::new(
                ScriptedPeer::new(vec![Err(DurableError::Timeout), Ok(()), Ok(())]),
                "127.0.0.1:1".parse().unwrap(),
            )),
            OffsetMode::Dynamo(MemoryOffsetPublisher::new()),
        );
        let mut first_offsets = HashMap::new();
        first_offsets.insert(OffsetKey::new("ns", "a"), 1);
        assert!(store
            .commit_segment(
                "seg-a",
                &first_offsets,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .is_err());
        let mut second_offsets = HashMap::new();
        second_offsets.insert(OffsetKey::new("ns", "b"), 1);
        store
            .commit_segment(
                "seg-b",
                &second_offsets,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let reopened = MutationLog::open(paths).unwrap();
        assert_eq!(reopened.committed_index().get(), 2);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn replicate_definite_nack_aborts_prepared() {
        use crate::buffer::durable::replicate::QuorumReplicator;
        use crate::cluster::peer::ScriptedPeer;

        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let guard = leased_guard(key.clone());
        let log = MutationLog::open(paths.clone()).unwrap();
        let store = PipelineDurableStore::new(
            key,
            paths.clone(),
            guard,
            log,
            ReplicationMode::Synchronous(QuorumReplicator::new(
                ScriptedPeer::new(vec![Err(DurableError::StaleEpoch)]),
                "127.0.0.1:1".parse().unwrap(),
            )),
            OffsetMode::Dynamo(MemoryOffsetPublisher::new()),
        );
        let err = store.commit_reclaim_segment("s".into()).await.unwrap_err();
        assert!(matches!(err, DurableError::StaleEpoch));
        let reopened = MutationLog::open(paths).unwrap();
        assert!(reopened.pending_prepared().is_empty());
    }

    #[tokio::test]
    async fn reconcile_while_owner_elect_publishes_without_activate() {
        let dir = tempfile::tempdir().unwrap();
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::new(dir.path(), &key).unwrap();
        let guard = owner_elect_guard(key.clone());
        let log = MutationLog::open(paths.clone()).unwrap();
        let publisher = MemoryOffsetPublisher::new();
        let store = PipelineDurableStore::new(
            key,
            paths,
            guard.clone(),
            log,
            ReplicationMode::LocalOnly,
            OffsetMode::Dynamo(publisher.clone()),
        );
        store.reconcile_published_offsets().await.unwrap();
        assert_eq!(
            guard.lifecycle(),
            skippr_lease::PipelineLifecycle::OwnerElect
        );
        assert_eq!(publisher.published.lock().await.len(), 1);
        assert!(store.commit_reclaim_segment("s".into()).await.is_err());
    }
}
