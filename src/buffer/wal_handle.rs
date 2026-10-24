//! Catalog-backed pipeline WAL handle. The store catalog is the only
//! authority for live segments; payload files and objects hold bytes only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Instant, SystemTime};

use skippr_lease::{
    DurableError, LeaseGuard, OffsetPublishError, PipelineKey, PipelinePaths, SegmentId,
};
use skippr_wal_catalog::{
    query_all, CatalogFence, CatalogKey, CatalogPrefix, CatalogRow, CatalogSeq, CatalogTxn,
    CheckpointKey, CommittedCheckpoint, CommittedOffset, ControlBody, ControlRow, FormatPhase,
    FormatRow, MemoryWalCatalog, OffsetHintKey, PayloadLocation, PutCond, SegmentIndexRow,
    SegmentRow, SeqRow, SliceBitmap, SliceDirEntry, SliceIndexEntry, WalCatalogStore,
    FORMAT_VERSION,
};
use tokio::sync::Mutex;

use crate::buffer::durable::replicate::ReplicationMode;
use crate::buffer::segment_file::{encode_snapshot, PartitionKey, SegmentFile, WalPayload};
use crate::buffer::wal_store::{SegmentWriteLocation, SegmentWriteResult};
use crate::helpers::offset_store::{publish_committed, OffsetStore};
use crate::helpers::offsets::OffsetKey;
use crate::plugins::cdc::CheckpointEnvelope;

/// Bound on catalog commits that have not yet published. Matches the WAL writer
/// channel cap class: not an env knob.
pub const UNPUBLISHED_COMMIT_CEILING: u64 = 32;
/// Shared progress cadence for import, hydrate, redo, and republish. Not an env knob.
pub(crate) const WAL_CATALOG_PROGRESS_EVERY: u64 = 250;
/// Durable Acked cleanup batch. Stays under TXN_MAX_OPS. Not an env knob.
pub(crate) const REDO_ACKED_BATCH: usize = 32;

pub struct PipelineWalHandle {
    key: PipelineKey,
    paths: PipelinePaths,
    guard: Arc<LeaseGuard>,
    catalog: Arc<dyn WalCatalogStore>,
    offsets: Arc<dyn OffsetStore>,
    replicator: ReplicationMode,
    seq: Mutex<SeqState>,
    catalog_mutex: Mutex<()>,
    in_memory_done: std::sync::Mutex<HashMap<String, SliceBitmap>>,
}

struct SeqState {
    max_committed: CatalogSeq,
    max_published: CatalogSeq,
}

static ACTIVE: OnceLock<RwLock<HashMap<PipelineKey, Arc<PipelineWalHandle>>>> = OnceLock::new();

fn active_slot() -> &'static RwLock<HashMap<PipelineKey, Arc<PipelineWalHandle>>> {
    ACTIVE.get_or_init(|| RwLock::new(HashMap::new()))
}

pub fn wal_handle_for(key: &PipelineKey) -> Option<Arc<PipelineWalHandle>> {
    active_slot()
        .read()
        .ok()
        .and_then(|map| map.get(key).cloned())
}

pub fn all_wal_handles() -> Vec<Arc<PipelineWalHandle>> {
    active_slot()
        .read()
        .ok()
        .map(|map| map.values().cloned().collect())
        .unwrap_or_default()
}

pub fn install_wal_handle(handle: Arc<PipelineWalHandle>) {
    if let Ok(mut map) = active_slot().write() {
        map.insert(handle.key.clone(), handle);
    }
}

pub fn remove_wal_handle(key: &PipelineKey) {
    if let Ok(mut map) = active_slot().write() {
        map.remove(key);
    }
}

pub fn clear_wal_handles() {
    if let Ok(mut map) = active_slot().write() {
        map.clear();
    }
}

impl PipelineWalHandle {
    pub fn new(
        key: PipelineKey,
        paths: PipelinePaths,
        guard: Arc<LeaseGuard>,
        catalog: Arc<dyn WalCatalogStore>,
        offsets: Arc<dyn OffsetStore>,
    ) -> Arc<Self> {
        Self::with_replication(
            key,
            paths,
            guard,
            catalog,
            offsets,
            ReplicationMode::LocalOnly,
        )
    }

    pub fn with_replication(
        key: PipelineKey,
        paths: PipelinePaths,
        guard: Arc<LeaseGuard>,
        catalog: Arc<dyn WalCatalogStore>,
        offsets: Arc<dyn OffsetStore>,
        replicator: ReplicationMode,
    ) -> Arc<Self> {
        Arc::new(Self {
            key,
            paths,
            guard,
            catalog,
            offsets,
            replicator,
            seq: Mutex::new(SeqState {
                max_committed: CatalogSeq::ZERO,
                max_published: CatalogSeq::ZERO,
            }),
            catalog_mutex: Mutex::new(()),
            in_memory_done: std::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn key(&self) -> &PipelineKey {
        &self.key
    }

    pub fn paths(&self) -> &PipelinePaths {
        &self.paths
    }

    pub fn guard(&self) -> &Arc<LeaseGuard> {
        &self.guard
    }

    pub fn catalog(&self) -> &dyn WalCatalogStore {
        self.catalog.as_ref()
    }

    pub fn fence(&self) -> Result<CatalogFence, DurableError> {
        catalog_fence_from_guard(&self.guard)
    }

    pub fn catalog_seq_seen(&self) -> Result<u64, DurableError> {
        match self
            .catalog
            .get(&CatalogKey::Seq)
            .map_err(|err| DurableError::Io(err.to_string()))?
        {
            Some(CatalogRow::Seq(row)) => Ok(row.max_seq.get()),
            Some(_) => Err(DurableError::ProtocolMismatch(
                "wal#seq is not a SeqRow".into(),
            )),
            None => Ok(0),
        }
    }

    pub fn payload_ready(&self) -> Result<bool, DurableError> {
        let rows = query_all(self.catalog.as_ref(), CatalogPrefix::Segment)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        for (_, row) in rows {
            let CatalogRow::Segment(seg) = row else {
                continue;
            };
            if !payload_present(&seg.location) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn payload_unreadable(&self, segment_id: &str) -> bool {
        let Ok(id) = SegmentId::new(segment_id) else {
            return true;
        };
        match self.catalog.get(&CatalogKey::Segment(id)) {
            Ok(Some(CatalogRow::Segment(seg))) => {
                admit_payload(&seg.location, &seg.payload_sha256).is_err()
            }
            _ => true,
        }
    }

    pub async fn live_wal_segments(&self) -> Result<crate::plugins::LiveWalSegments, DurableError> {
        Ok(crate::plugins::LiveWalSegments::new(
            self.catalog_segment_ids()?,
        ))
    }

    pub fn live_disk_payload_paths(&self) -> Result<Vec<PathBuf>, DurableError> {
        let rows = query_all(self.catalog.as_ref(), CatalogPrefix::Segment)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let mut paths = Vec::new();
        for (_, row) in rows {
            if let CatalogRow::Segment(seg) = row {
                if let PayloadLocation::Disk { path } = seg.location {
                    paths.push(PathBuf::from(path));
                }
            }
        }
        Ok(paths)
    }

    pub fn catalog_segment_ids(&self) -> Result<Vec<String>, DurableError> {
        Ok(self
            .live_segment_rows()?
            .into_iter()
            .map(|seg| seg.segment_id.as_str().to_string())
            .collect())
    }

    pub fn live_segment_rows(&self) -> Result<Vec<SegmentRow>, DurableError> {
        let rows = query_all(self.catalog.as_ref(), CatalogPrefix::Segment)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        Ok(rows
            .into_iter()
            .filter_map(|(_, row)| match row {
                CatalogRow::Segment(seg) => Some(seg),
                _ => None,
            })
            .collect())
    }

    pub fn select_live_ordinals(&self, exclude_segment_ids: &[String]) -> Vec<(String, u32)> {
        self.live_slices(exclude_segment_ids)
            .into_iter()
            .map(|slice| (slice.segment_id.as_str().to_string(), slice.ordinal))
            .collect()
    }

    pub fn live_slices(&self, exclude_segment_ids: &[String]) -> Vec<SliceIndexEntry> {
        let exclude: std::collections::HashSet<&str> =
            exclude_segment_ids.iter().map(String::as_str).collect();
        let Ok(rows) = query_all(self.catalog.as_ref(), CatalogPrefix::Index) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (_, row) in rows {
            let CatalogRow::Index(index) = row else {
                continue;
            };
            let id = index.segment_id.as_str();
            if exclude.contains(id) {
                continue;
            }
            let Ok(Some(CatalogRow::Segment(seg))) = self
                .catalog
                .get(&CatalogKey::Segment(index.segment_id.clone()))
            else {
                continue;
            };
            if !payload_present(&seg.location) {
                tracing::warn!(segment = id, "live WAL scan skipped missing payload");
                continue;
            }
            for entry in index.entries {
                out.push(slice_index_from_dir(&index.segment_id, &entry));
            }
        }
        out
    }

    pub fn slice_is_done(&self, segment_id: &str, ordinal: u32) -> Result<bool, DurableError> {
        if let Ok(mem) = self.in_memory_done.lock() {
            if let Some(bits) = mem.get(segment_id) {
                if bits.is_done(ordinal) {
                    return Ok(true);
                }
            }
        }
        let id = SegmentId::new(segment_id).map_err(|err| DurableError::Io(err.to_string()))?;
        match self
            .catalog
            .get(&CatalogKey::Segment(id))
            .map_err(|err| DurableError::Io(err.to_string()))?
        {
            Some(CatalogRow::Segment(row)) => Ok(row.done.is_done(ordinal)),
            Some(_) => Err(DurableError::ProtocolMismatch(
                "wal#seg is not a SegmentRow".into(),
            )),
            None => Ok(true),
        }
    }

    pub fn hydrate_in_memory_done(&self) -> Result<(), DurableError> {
        let rows = self.live_segment_rows()?;
        let mut mem = self
            .in_memory_done
            .lock()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        mem.clear();
        for row in rows {
            mem.insert(row.segment_id.as_str().to_string(), row.done);
        }
        Ok(())
    }

    pub fn mark_acked_ordinals_in_memory(
        &self,
        segment_id: &str,
        ordinals: &[u32],
        slice_count: u32,
    ) {
        if let Ok(mut mem) = self.in_memory_done.lock() {
            let bits = mem
                .entry(segment_id.to_string())
                .or_insert_with(|| SliceBitmap::new(slice_count));
            bits.or_bits(ordinals);
        }
    }

    pub fn forget_in_memory_segment(&self, segment_id: &str) {
        if let Ok(mut mem) = self.in_memory_done.lock() {
            mem.remove(segment_id);
        }
    }

    pub fn compaction_transactions(
        &self,
    ) -> Result<Vec<crate::buffer::compaction_transaction::CompactionTransaction>, DurableError>
    {
        use crate::buffer::compaction_transaction::{
            CompactionTransaction, CompactionTransactionState,
        };
        let rows = query_all(self.catalog.as_ref(), CatalogPrefix::Compaction)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let mut out = Vec::new();
        for (_, row) in rows {
            let CatalogRow::Compaction(compaction) = row else {
                continue;
            };
            let mut txn: CompactionTransaction = serde_json::from_slice(&compaction.body)
                .map_err(|err| DurableError::Io(err.to_string()))?;
            txn.state = match compaction.state {
                skippr_wal_catalog::CompactionState::Pending => CompactionTransactionState::Pending,
                skippr_wal_catalog::CompactionState::Sent => CompactionTransactionState::Sent,
                skippr_wal_catalog::CompactionState::Acked => CompactionTransactionState::Acked,
            };
            out.push(txn);
        }
        Ok(out)
    }

    fn reclaim_epoch(&self) -> Result<skippr_lease::LeaseEpoch, DurableError> {
        self.guard
            .require_active_epoch()
            .or_else(|_| self.guard.require_recovery_epoch())
            .map_err(DurableError::from)
    }

    pub fn apply_acked_in_memory(&self) -> Result<Vec<String>, DurableError> {
        use crate::buffer::compaction_transaction::CompactionTransactionState;
        self.hydrate_in_memory_done()?;
        let txns = self.compaction_transactions()?;
        let mut acked_ids = Vec::new();
        for txn in txns {
            let bits_done = self.all_refs_already_done(&txn).unwrap_or(false);
            if txn.state != CompactionTransactionState::Acked && !bits_done {
                continue;
            }
            for wal_ref in &txn.refs {
                let Some(ordinal) = self.resolve_ordinal(wal_ref)? else {
                    continue;
                };
                let slice_count = self
                    .segment_slice_count(&wal_ref.segment_id)
                    .unwrap_or(ordinal + 1);
                self.mark_acked_ordinals_in_memory(&wal_ref.segment_id, &[ordinal], slice_count);
            }
            acked_ids.push(txn.id.clone());
        }
        Ok(acked_ids)
    }

    pub fn spawn_durable_redo(self: &Arc<Self>, acked: Vec<String>) {
        if acked.is_empty() {
            return;
        }
        let handle = Arc::clone(self);
        tokio::spawn(async move {
            let _ = handle.redo_acked_durable_batches(&acked).await;
        });
    }

    pub async fn redo_acked_compactions(&self) -> Result<(), DurableError> {
        let _ = self.reclaim_epoch()?;
        let started = Instant::now();
        let acked = self.apply_acked_in_memory()?;
        let compactions = acked.len();
        tracing::info!(compactions, "WAL catalog redo_acked begin");
        self.redo_acked_durable_batches(&acked).await?;
        tracing::info!(
            scanned = compactions,
            redone = compactions,
            deferred = 0u64,
            compactions,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "WAL catalog redo_acked complete"
        );
        Ok(())
    }

    pub async fn redo_acked_durable_batches(
        &self,
        acked_ids: &[String],
    ) -> Result<(), DurableError> {
        let txns = self.compaction_transactions()?;
        let wanted: std::collections::HashSet<&str> =
            acked_ids.iter().map(String::as_str).collect();
        let mut batch = Vec::new();
        let mut scanned = 0u64;
        let mut redone = 0u64;
        let mut deferred = 0u64;
        let mut first_deferred: Option<String> = None;
        let total = wanted.len() as u64;
        for txn in txns {
            if !wanted.contains(txn.id.as_str()) {
                continue;
            }
            batch.push(txn);
            if batch.len() < REDO_ACKED_BATCH {
                continue;
            }
            scanned += batch.len() as u64;
            match self.flush_acked_batch(&batch).await {
                Ok(n) => redone += n,
                Err(err) => {
                    deferred += batch.len() as u64;
                    if first_deferred.is_none() {
                        first_deferred = Some(format!("{err}"));
                    }
                }
            }
            if scanned % WAL_CATALOG_PROGRESS_EVERY == 0 {
                tracing::info!(
                    scanned,
                    redone,
                    deferred,
                    compactions = total,
                    "WAL catalog redo_acked progress"
                );
            }
            batch.clear();
        }
        if !batch.is_empty() {
            scanned += batch.len() as u64;
            match self.flush_acked_batch(&batch).await {
                Ok(n) => redone += n,
                Err(err) => {
                    deferred += batch.len() as u64;
                    if first_deferred.is_none() {
                        first_deferred = Some(format!("{err}"));
                    }
                }
            }
        }
        if deferred > 0 {
            tracing::warn!(
                deferred,
                first_error = first_deferred.as_deref().unwrap_or(""),
                "WAL catalog redo_acked deferred some rows; catalog kept"
            );
        }
        let _ = (redone, scanned);
        Ok(())
    }

    async fn flush_acked_batch(
        &self,
        batch: &[crate::buffer::compaction_transaction::CompactionTransaction],
    ) -> Result<u64, DurableError> {
        let epoch = self.reclaim_epoch()?;
        let fence = self.fence()?;
        let mut grouped: HashMap<String, Vec<u32>> = HashMap::new();
        for txn in batch {
            for wal_ref in &txn.refs {
                let Some(ordinal) = self.resolve_ordinal(wal_ref)? else {
                    continue;
                };
                grouped
                    .entry(wal_ref.segment_id.clone())
                    .or_default()
                    .push(ordinal);
            }
        }
        let mut catalog_txn = CatalogTxn::new();
        let mut reclaim_after: Vec<(String, PayloadLocation)> = Vec::new();
        for (segment_id, ordinals) in &grouped {
            let id = SegmentId::new(segment_id).map_err(|err| DurableError::Io(err.to_string()))?;
            let Some(CatalogRow::Segment(mut row)) = self
                .catalog
                .get(&CatalogKey::Segment(id.clone()))
                .map_err(|err| DurableError::Io(err.to_string()))?
            else {
                continue;
            };
            row.done.or_bits(ordinals);
            crate::metrics::counters::add_compaction_catalog_bit_write(ordinals.len() as u64);
            let all_done = row.done.all_done();
            let version = row.version;
            row.version = version.saturating_add(1);
            let location = row.location.clone();
            if all_done {
                catalog_txn
                    .delete(
                        CatalogKey::Segment(id.clone()),
                        skippr_wal_catalog::DeleteCond::Version(version),
                    )
                    .map_err(|err| DurableError::Io(err.to_string()))?;
                if self
                    .catalog
                    .get(&CatalogKey::Index(id.clone()))
                    .map_err(|err| DurableError::Io(err.to_string()))?
                    .is_some()
                {
                    catalog_txn
                        .delete(
                            CatalogKey::Index(id.clone()),
                            skippr_wal_catalog::DeleteCond::Exists,
                        )
                        .map_err(|err| DurableError::Io(err.to_string()))?;
                }
                reclaim_after.push((segment_id.clone(), location));
            } else {
                catalog_txn
                    .put(
                        CatalogKey::Segment(id),
                        CatalogRow::Segment(row),
                        PutCond::Version(version),
                    )
                    .map_err(|err| DurableError::Io(err.to_string()))?;
            }
        }
        for txn in batch {
            let compaction_id = skippr_wal_catalog::CompactionId::new(&txn.id)
                .map_err(|err| DurableError::Io(err.to_string()))?;
            if self
                .catalog
                .get(&CatalogKey::Compaction(compaction_id.clone()))
                .map_err(|err| DurableError::Io(err.to_string()))?
                .is_some()
            {
                catalog_txn
                    .delete(
                        CatalogKey::Compaction(compaction_id),
                        skippr_wal_catalog::DeleteCond::Exists,
                    )
                    .map_err(|err| DurableError::Io(err.to_string()))?;
            }
        }
        if !catalog_txn.ops().is_empty() {
            self.commit_durable(&catalog_txn, fence)?;
        }
        for (segment_id, location) in reclaim_after {
            delete_payload(&location)?;
            if let ReplicationMode::Synchronous(replicator) = &self.replicator {
                let _ = crate::cluster::peer::drop_payload_at(
                    replicator.snapshot_endpoint(),
                    &self.key,
                    &segment_id,
                    epoch.get(),
                    &crate::cluster::identity::process_query_bind()
                        .map(|bind| bind.identity)
                        .ok_or_else(|| DurableError::Io("cluster identity missing".into()))?,
                )
                .await;
            }
            self.forget_in_memory_segment(&segment_id);
            crate::buffer::ingest_buffer::Buffers::forget_reclaimed_segment(&segment_id);
            tracing::info!(segment = segment_id.as_str(), "WAL catalog reclaim");
        }
        Ok(batch.len() as u64)
    }

    fn segment_slice_count(&self, segment_id: &str) -> Result<u32, DurableError> {
        let id = SegmentId::new(segment_id).map_err(|err| DurableError::Io(err.to_string()))?;
        match self
            .catalog
            .get(&CatalogKey::Segment(id))
            .map_err(|err| DurableError::Io(err.to_string()))?
        {
            Some(CatalogRow::Segment(row)) => Ok(row.slice_count),
            _ => Ok(1),
        }
    }

    fn all_refs_already_done(
        &self,
        txn: &crate::buffer::compaction_transaction::CompactionTransaction,
    ) -> Result<bool, DurableError> {
        if txn.refs.is_empty() {
            return Ok(true);
        }
        for wal_ref in &txn.refs {
            let Some(ordinal) = self.resolve_ordinal(wal_ref)? else {
                return Ok(false);
            };
            if !self.slice_is_done(&wal_ref.segment_id, ordinal)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn resolve_ordinal(
        &self,
        wal_ref: &crate::buffer::compaction_transaction::WalPartRef,
    ) -> Result<Option<u32>, DurableError> {
        if let Some(ordinal) = wal_ref.ordinal {
            return Ok(Some(ordinal));
        }
        let id =
            SegmentId::new(&wal_ref.segment_id).map_err(|err| DurableError::Io(err.to_string()))?;
        if let Some(CatalogRow::Index(index)) = self
            .catalog
            .get(&CatalogKey::Index(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?
        {
            for entry in &index.entries {
                if entry.start == wal_ref.start
                    && entry.len == wal_ref.len
                    && entry.namespace == wal_ref.key.namespace
                    && entry.partition == wal_ref.key.partition
                {
                    return Ok(Some(entry.ordinal));
                }
            }
        }
        let Some(CatalogRow::Segment(seg)) = self
            .catalog
            .get(&CatalogKey::Segment(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?
        else {
            return Ok(None);
        };
        let PayloadLocation::Disk { path } = &seg.location else {
            return Ok(None);
        };
        let meta = SegmentFile::read_index_path(std::path::Path::new(path))
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let _ = self.persist_missing_index(&id, &meta);
        for idx in &meta.index {
            if idx.start == wal_ref.start
                && idx.len == wal_ref.len
                && idx.key.namespace == wal_ref.key.namespace
                && idx.key.partition == wal_ref.key.partition
            {
                return Ok(Some(idx.slice_ordinal));
            }
        }
        Ok(None)
    }

    fn persist_missing_index(
        &self,
        id: &SegmentId,
        meta: &crate::buffer::segment_file::SegmentFileMetadata,
    ) -> Result<(), DurableError> {
        if self
            .catalog
            .get(&CatalogKey::Index(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?
            .is_some()
        {
            return Ok(());
        }
        let fence = self.fence()?;
        let mut txn = CatalogTxn::new();
        put_slice_index(&mut txn, id, meta)?;
        if txn.ops().is_empty() {
            return Ok(());
        }
        self.commit_durable(&txn, fence)
    }

    pub fn load_or_build_index_meta(
        &self,
        row: &SegmentRow,
    ) -> Result<crate::buffer::segment_file::SegmentFileMetadata, DurableError> {
        let id = row.segment_id.clone();
        if let Some(CatalogRow::Index(index)) = self
            .catalog
            .get(&CatalogKey::Index(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?
        {
            return Ok(meta_from_index_row(row, &index));
        }
        let meta = read_index_location(&row.location)?;
        let _ = self.persist_missing_index(&id, &meta);
        Ok(meta)
    }

    pub async fn persist_segment(
        &self,
        snapshot_id: &str,
        offsets: &HashMap<OffsetKey, u64>,
        batches: &HashMap<PartitionKey, WalPayload>,
        partitions_meta: &HashMap<PartitionKey, (u64, SystemTime)>,
        part_meta_blobs: &HashMap<PartitionKey, Vec<u8>>,
        checkpoint_updates: &HashMap<String, CheckpointEnvelope>,
    ) -> Result<SegmentWriteResult, DurableError> {
        let epoch = self.guard.require_active_epoch()?;
        let fence = self.fence()?;
        let id = SegmentId::new(snapshot_id).map_err(|err| DurableError::Io(err.to_string()))?;
        std::fs::create_dir_all(&self.paths.segs).map_err(DurableError::from)?;
        let encoded = encode_snapshot(offsets, batches, partitions_meta, part_meta_blobs)
            .map_err(DurableError::from)?;
        let seg_file = SegmentFile::new(&self.paths.segs, snapshot_id)?;
        {
            let mut durable = crate::buffer::direct_io::DirectIoFile::create(&seg_file.path)?;
            use std::io::Write;
            durable.write_all(&encoded.bytes)?;
            durable.sync_data()?;
        }
        if let ReplicationMode::Synchronous(replicator) = &self.replicator {
            replicator
                .commit_payload(
                    snapshot_id,
                    encoded.sha256,
                    encoded.bytes.len() as u64,
                    epoch.get(),
                    &seg_file.path,
                    self.guard.as_ref(),
                )
                .await?;
            crate::cluster::failpoint::hit(
                crate::cluster::failpoint::FailpointName::AfterReplicaPayloadAck,
                &self.paths.root,
            )?;
        }
        let location = PayloadLocation::Disk {
            path: seg_file.path.to_string_lossy().into_owned(),
        };
        let committed_offsets = committed_offsets_from(offsets, self.offsets.as_ref())?;
        let checkpoints = checkpoints_from(checkpoint_updates)?;
        let slice_count = encoded.meta.num_partitions.max(1);
        let existing = self
            .catalog
            .get(&CatalogKey::Segment(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let seq = if let Some(CatalogRow::Segment(row)) = existing {
            if row.payload_sha256 != encoded.sha256 {
                return Err(DurableError::ProtocolMismatch(
                    "segment_id reuse with different payload sha".into(),
                ));
            }
            row.seq
        } else {
            self.commit_new_segment(
                &id,
                &encoded,
                &location,
                slice_count,
                committed_offsets.clone(),
                checkpoints.clone(),
                fence,
                epoch,
            )
            .await?
        };
        self.publish_seq(seq, epoch, &committed_offsets, &checkpoints)
            .await?;
        Ok(SegmentWriteResult {
            meta: encoded.meta,
            total_rows: encoded.total_rows,
            sha256: encoded.sha256,
            location: SegmentWriteLocation::Disk {
                path: seg_file.path.clone(),
            },
            offsets_published: true,
        })
    }

    async fn commit_new_segment(
        &self,
        id: &SegmentId,
        encoded: &crate::buffer::segment_file::EncodedSnapshot,
        location: &PayloadLocation,
        slice_count: u32,
        committed_offsets: Vec<CommittedOffset>,
        checkpoints: Vec<CommittedCheckpoint>,
        fence: CatalogFence,
        epoch: skippr_lease::LeaseEpoch,
    ) -> Result<CatalogSeq, DurableError> {
        let _catalog = self.catalog_mutex.lock().await;
        {
            let seq = self.seq.lock().await;
            if seq
                .max_committed
                .get()
                .saturating_sub(seq.max_published.get())
                >= UNPUBLISHED_COMMIT_CEILING
            {
                return Err(DurableError::Io(
                    "unpublished WAL catalog commit ceiling reached".into(),
                ));
            }
        }
        crate::cluster::failpoint::hit(
            crate::cluster::failpoint::FailpointName::BeforeCatalogCommit,
            &self.paths.root,
        )?;
        let max_seq = match self
            .catalog
            .get(&CatalogKey::Seq)
            .map_err(|err| DurableError::Io(err.to_string()))?
        {
            Some(CatalogRow::Seq(row)) => row.max_seq,
            Some(_) => {
                return Err(DurableError::ProtocolMismatch(
                    "wal#seq is not a SeqRow".into(),
                ))
            }
            None => CatalogSeq::ZERO,
        };
        let seq = max_seq
            .next()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let row = SegmentRow {
            segment_id: id.clone(),
            seq,
            location: location.clone(),
            payload_len: encoded.meta.total_bytes,
            payload_sha256: encoded.sha256,
            slice_count,
            done: SliceBitmap::new(slice_count),
            offsets: committed_offsets,
            checkpoints,
            epoch,
            version: 0,
        };
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Segment(id.clone()),
            CatalogRow::Segment(row),
            PutCond::NotExists,
        )
        .map_err(|err| DurableError::Io(err.to_string()))?;
        put_slice_index(&mut txn, id, &encoded.meta)?;
        txn.put(
            CatalogKey::Seq,
            CatalogRow::Seq(SeqRow { max_seq: seq }),
            PutCond::SeqAdvance,
        )
        .map_err(|err| DurableError::Io(err.to_string()))?;
        self.commit_durable(&txn, fence)?;
        crate::cluster::failpoint::hit(
            crate::cluster::failpoint::FailpointName::AfterCatalogCommit,
            &self.paths.root,
        )?;
        self.seq.lock().await.max_committed = seq;
        Ok(seq)
    }

    fn commit_durable(&self, txn: &CatalogTxn, fence: CatalogFence) -> Result<(), DurableError> {
        flushing_commit(self.catalog.as_ref(), txn, fence)
    }

    async fn publish_seq(
        &self,
        seq: CatalogSeq,
        epoch: skippr_lease::LeaseEpoch,
        offsets: &[CommittedOffset],
        checkpoints: &[CommittedCheckpoint],
    ) -> Result<(), DurableError> {
        let mut delay = std::time::Duration::from_millis(50);
        loop {
            if self.guard.require_same_offset_epoch(epoch).is_err() {
                return Err(DurableError::FencedAfterCommit);
            }
            match publish_committed(
                self.offsets.as_ref(),
                offsets,
                checkpoints,
                epoch.get(),
                seq.get(),
                skippr_lease::PublishMode::Fresh,
            ) {
                Ok(()) => {
                    self.offsets.flush().map_err(|err| DurableError::Io(err))?;
                    crate::cluster::failpoint::hit(
                        crate::cluster::failpoint::FailpointName::AfterOffsetsPublished,
                        &self.paths.root,
                    )?;
                    self.seq.lock().await.max_published = seq;
                    return Ok(());
                }
                Err(OffsetPublishError::Corrupt(err)) => {
                    return Err(DurableError::CorruptOffset(err));
                }
                Err(OffsetPublishError::FenceRegression) => {
                    return Err(DurableError::FencedAfterCommit);
                }
                Err(OffsetPublishError::Transient(_)) => {
                    if self.guard.sleep_or_fence(delay).await.is_err() {
                        return Err(DurableError::FencedAfterCommit);
                    }
                    delay = (delay * 2).min(std::time::Duration::from_secs(2));
                }
            }
        }
    }

    pub async fn persist_s3_segment(
        &self,
        snapshot_id: &str,
        encoded: &crate::buffer::segment_file::EncodedSnapshot,
        bucket: &str,
        object_key: &str,
        offsets: &HashMap<OffsetKey, u64>,
        checkpoint_updates: &HashMap<String, CheckpointEnvelope>,
    ) -> Result<SegmentWriteResult, DurableError> {
        let epoch = self.guard.require_active_epoch()?;
        let fence = self.fence()?;
        let id = SegmentId::new(snapshot_id).map_err(|err| DurableError::Io(err.to_string()))?;
        let location = PayloadLocation::S3 {
            bucket: bucket.to_string(),
            key: object_key.to_string(),
        };
        let committed_offsets = committed_offsets_from(offsets, self.offsets.as_ref())?;
        let checkpoints = checkpoints_from(checkpoint_updates)?;
        let slice_count = encoded.meta.num_partitions.max(1);
        let existing = self
            .catalog
            .get(&CatalogKey::Segment(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let seq = if let Some(CatalogRow::Segment(row)) = existing {
            if row.payload_sha256 != encoded.sha256 {
                return Err(DurableError::ProtocolMismatch(
                    "segment_id reuse with different payload sha".into(),
                ));
            }
            row.seq
        } else {
            self.commit_new_segment(
                &id,
                encoded,
                &location,
                slice_count,
                committed_offsets.clone(),
                checkpoints.clone(),
                fence,
                epoch,
            )
            .await?
        };
        self.publish_seq(seq, epoch, &committed_offsets, &checkpoints)
            .await?;
        Ok(SegmentWriteResult {
            meta: encoded.meta.clone(),
            total_rows: encoded.total_rows,
            sha256: encoded.sha256,
            location: SegmentWriteLocation::S3 {
                key: object_key.to_string(),
                bucket: bucket.to_string(),
            },
            offsets_published: true,
        })
    }

    pub async fn persist_checkpoint(
        &self,
        logical_key: &str,
        envelope: &CheckpointEnvelope,
    ) -> Result<(), DurableError> {
        let bytes =
            bincode::serialize(envelope).map_err(|err| DurableError::Io(err.to_string()))?;
        let key = CatalogKey::Checkpoint(
            CheckpointKey::new(logical_key).map_err(|err| DurableError::Io(err.to_string()))?,
        );
        let body = ControlBody::Checkpoint {
            logical_key: logical_key.to_string(),
            envelope: bytes.clone(),
        };
        self.persist_control(
            key,
            body,
            Vec::new(),
            vec![CommittedCheckpoint {
                logical_key: logical_key.to_string(),
                envelope: bytes,
            }],
        )
        .await
    }

    pub async fn persist_offset_hint(
        &self,
        namespace: &str,
        partition: &str,
        position: u64,
        closed: bool,
    ) -> Result<(), DurableError> {
        let key = CatalogKey::OffsetHint(
            OffsetHintKey::new(namespace, partition)
                .map_err(|err| DurableError::Io(err.to_string()))?,
        );
        let body = ControlBody::OffsetHint { position, closed };
        let existing = self
            .offsets
            .get_offset(namespace, partition)
            .map_err(DurableError::Io)?;
        let filesize = match existing.as_deref() {
            Some(bytes) if bytes.len() == 24 => u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            _ => 0,
        };
        self.persist_control(
            key,
            body,
            vec![CommittedOffset {
                namespace: namespace.to_string(),
                partition: partition.to_string(),
                filesize,
                position,
                closed: if closed { 1 } else { 0 },
            }],
            Vec::new(),
        )
        .await
    }

    async fn persist_control(
        &self,
        key: CatalogKey,
        body: ControlBody,
        offsets: Vec<CommittedOffset>,
        checkpoints: Vec<CommittedCheckpoint>,
    ) -> Result<(), DurableError> {
        let epoch = self.guard.require_active_epoch()?;
        let fence = self.fence()?;
        let existing = self
            .catalog
            .get(&key)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let seq = if let Some(CatalogRow::Control(row)) = existing {
            if row.body == body {
                row.seq
            } else {
                self.commit_control_row(&key, body, Some(row.version), fence, epoch)
                    .await?
            }
        } else {
            self.commit_control_row(&key, body, None, fence, epoch)
                .await?
        };
        self.publish_seq(seq, epoch, &offsets, &checkpoints).await
    }

    async fn commit_control_row(
        &self,
        key: &CatalogKey,
        body: ControlBody,
        existing_version: Option<u64>,
        fence: CatalogFence,
        epoch: skippr_lease::LeaseEpoch,
    ) -> Result<CatalogSeq, DurableError> {
        let _catalog = self.catalog_mutex.lock().await;
        {
            let seq = self.seq.lock().await;
            if seq
                .max_committed
                .get()
                .saturating_sub(seq.max_published.get())
                >= UNPUBLISHED_COMMIT_CEILING
            {
                return Err(DurableError::Io(
                    "unpublished WAL catalog commit ceiling reached".into(),
                ));
            }
        }
        let max_seq = match self
            .catalog
            .get(&CatalogKey::Seq)
            .map_err(|err| DurableError::Io(err.to_string()))?
        {
            Some(CatalogRow::Seq(row)) => row.max_seq,
            Some(_) => {
                return Err(DurableError::ProtocolMismatch(
                    "wal#seq is not a SeqRow".into(),
                ))
            }
            None => CatalogSeq::ZERO,
        };
        let seq = max_seq
            .next()
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let version = existing_version.map(|v| v.saturating_add(1)).unwrap_or(0);
        let cond = match existing_version {
            Some(v) => PutCond::Version(v),
            None => PutCond::NotExists,
        };
        let row = ControlRow {
            seq,
            epoch,
            body,
            version,
        };
        let mut txn = CatalogTxn::new();
        txn.put(key.clone(), CatalogRow::Control(row), cond)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        txn.put(
            CatalogKey::Seq,
            CatalogRow::Seq(SeqRow { max_seq: seq }),
            PutCond::SeqAdvance,
        )
        .map_err(|err| DurableError::Io(err.to_string()))?;
        self.commit_durable(&txn, fence)?;
        self.seq.lock().await.max_committed = seq;
        Ok(seq)
    }

    pub fn replace_payload_location(
        &self,
        segment_id: &str,
        location: PayloadLocation,
        payload_len: u64,
        payload_sha256: [u8; 32],
    ) -> Result<(), DurableError> {
        let _ = self.guard.require_active_epoch()?;
        let fence = self.fence()?;
        let id = SegmentId::new(segment_id).map_err(|err| DurableError::Io(err.to_string()))?;
        let Some(CatalogRow::Segment(mut row)) = self
            .catalog
            .get(&CatalogKey::Segment(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?
        else {
            return Err(DurableError::Io(format!(
                "missing SegmentRow for payload replace {segment_id}"
            )));
        };
        let version = row.version;
        row.location = location;
        row.payload_len = payload_len;
        row.payload_sha256 = payload_sha256;
        row.version = version.saturating_add(1);
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Segment(id),
            CatalogRow::Segment(row),
            PutCond::Version(version),
        )
        .map_err(|err| DurableError::Io(err.to_string()))?;
        self.commit_durable(&txn, fence)
    }

    pub async fn put_compaction(
        &self,
        id: &str,
        state: skippr_wal_catalog::CompactionState,
        body: Vec<u8>,
    ) -> Result<(), DurableError> {
        let _ = self.guard.require_active_epoch()?;
        let fence = self.fence()?;
        let compaction_id = skippr_wal_catalog::CompactionId::new(id)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let existing = self
            .catalog
            .get(&CatalogKey::Compaction(compaction_id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let (cond, version) = match existing {
            Some(CatalogRow::Compaction(row)) => {
                (PutCond::Version(row.version), row.version.saturating_add(1))
            }
            _ => (PutCond::NotExists, 0),
        };
        let row = skippr_wal_catalog::CompactionRow {
            id: compaction_id.clone(),
            state,
            body,
            version,
        };
        let mut txn = CatalogTxn::new();
        txn.put(
            CatalogKey::Compaction(compaction_id),
            CatalogRow::Compaction(row),
            cond,
        )
        .map_err(|err| DurableError::Io(err.to_string()))?;
        self.commit_durable(&txn, fence)
    }

    pub async fn complete_slices_and_reclaim(
        &self,
        segment_id: &str,
        ordinals: &[u32],
    ) -> Result<bool, DurableError> {
        let started = Instant::now();
        let epoch = self.reclaim_epoch()?;
        let fence = self.fence()?;
        let id = SegmentId::new(segment_id).map_err(|err| DurableError::Io(err.to_string()))?;
        let Some(CatalogRow::Segment(mut row)) = self
            .catalog
            .get(&CatalogKey::Segment(id.clone()))
            .map_err(|err| DurableError::Io(err.to_string()))?
        else {
            return Ok(true);
        };
        row.done.or_bits(ordinals);
        crate::metrics::counters::add_compaction_catalog_bit_write(ordinals.len() as u64);
        let all_done = row.done.all_done();
        let can_reclaim = all_done && crate::processor::allows_reclaim(row.seq.get());
        let version = row.version;
        row.version = version.saturating_add(1);
        let location = row.location.clone();
        let slice_count = row.slice_count;
        self.mark_acked_ordinals_in_memory(segment_id, ordinals, slice_count);
        let mut txn = CatalogTxn::new();
        if can_reclaim {
            txn.delete(
                CatalogKey::Segment(id.clone()),
                skippr_wal_catalog::DeleteCond::Version(version),
            )
            .map_err(|err| DurableError::Io(err.to_string()))?;
            if self
                .catalog
                .get(&CatalogKey::Index(id.clone()))
                .map_err(|err| DurableError::Io(err.to_string()))?
                .is_some()
            {
                txn.delete(
                    CatalogKey::Index(id.clone()),
                    skippr_wal_catalog::DeleteCond::Exists,
                )
                .map_err(|err| DurableError::Io(err.to_string()))?;
            }
        } else {
            txn.put(
                CatalogKey::Segment(id.clone()),
                CatalogRow::Segment(row),
                PutCond::Version(version),
            )
            .map_err(|err| DurableError::Io(err.to_string()))?;
        }
        self.commit_durable(&txn, fence)?;
        if can_reclaim {
            delete_payload(&location)?;
            if let ReplicationMode::Synchronous(replicator) = &self.replicator {
                let _ = crate::cluster::peer::drop_payload_at(
                    replicator.snapshot_endpoint(),
                    &self.key,
                    segment_id,
                    epoch.get(),
                    &crate::cluster::identity::process_query_bind()
                        .map(|bind| bind.identity)
                        .ok_or_else(|| DurableError::Io("cluster identity missing".into()))?,
                )
                .await;
            }
            self.forget_in_memory_segment(segment_id);
            crate::buffer::ingest_buffer::Buffers::forget_reclaimed_segment(segment_id);
            let elapsed_ms = started.elapsed().as_millis() as u64;
            tracing::info!(segment = segment_id, elapsed_ms, "WAL catalog reclaim");
            crate::helpers::perf::emit(
                "wal_reclaim",
                serde_json::json!({
                    "elapsed_ms": elapsed_ms,
                    "segment": segment_id
                }),
            );
        }
        Ok(can_reclaim)
    }

    /// Drop catalog rows and payloads for fully compacted segments the processor has passed.
    pub async fn reclaim_fully_done_before(
        &self,
        min_unconsumed_seq: u64,
    ) -> Result<u64, DurableError> {
        let rows = self.live_segment_rows()?;
        let mut reclaimed = 0u64;
        for row in rows {
            if !row.done.all_done() || row.seq.get() >= min_unconsumed_seq {
                continue;
            }
            if self
                .complete_slices_and_reclaim(row.segment_id.as_str(), &[])
                .await?
            {
                reclaimed += 1;
            }
        }
        Ok(reclaimed)
    }

    pub async fn delete_compaction(&self, id: &str) -> Result<(), DurableError> {
        let _ = self.reclaim_epoch()?;
        let fence = self.fence()?;
        let compaction_id = skippr_wal_catalog::CompactionId::new(id)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let mut txn = CatalogTxn::new();
        txn.delete(
            CatalogKey::Compaction(compaction_id),
            skippr_wal_catalog::DeleteCond::Exists,
        )
        .map_err(|err| DurableError::Io(err.to_string()))?;
        self.commit_durable(&txn, fence)
    }

    pub async fn republish_all(&self) -> Result<(), DurableError> {
        let started = Instant::now();
        let epoch = self.guard.require_offset_epoch()?;
        let rows = query_all(self.catalog.as_ref(), CatalogPrefix::Segment)
            .map_err(|err| DurableError::Io(err.to_string()))?;
        let mut ordered: Vec<SegmentRow> = rows
            .into_iter()
            .filter_map(|(_, row)| match row {
                CatalogRow::Segment(seg) => Some(seg),
                _ => None,
            })
            .collect();
        ordered.sort_by_key(|row| row.seq.get());
        let live_segments = ordered.len();
        tracing::info!(segments = live_segments, "WAL catalog republish begin");
        let mut segments = 0u64;
        for row in ordered {
            publish_committed(
                self.offsets.as_ref(),
                &row.offsets,
                &row.checkpoints,
                epoch.get(),
                row.seq.get(),
                skippr_lease::PublishMode::Republish,
            )
            .map_err(|err| DurableError::Io(err.to_string()))?;
            segments += 1;
            if segments % WAL_CATALOG_PROGRESS_EVERY == 0 {
                tracing::info!(segments, live_segments, "WAL catalog republish progress");
            }
        }
        let mut controls: Vec<(CatalogKey, ControlRow)> = Vec::new();
        for prefix in [CatalogPrefix::Checkpoint, CatalogPrefix::OffsetHint] {
            let rows = query_all(self.catalog.as_ref(), prefix)
                .map_err(|err| DurableError::Io(err.to_string()))?;
            for (key, row) in rows {
                if let CatalogRow::Control(control) = row {
                    controls.push((key, control));
                }
            }
        }
        controls.sort_by_key(|(_, row)| row.seq.get());
        let control_count = controls.len();
        for (key, row) in controls {
            match row.body {
                ControlBody::Checkpoint {
                    logical_key,
                    envelope,
                } => {
                    publish_committed(
                        self.offsets.as_ref(),
                        &[],
                        &[CommittedCheckpoint {
                            logical_key,
                            envelope,
                        }],
                        epoch.get(),
                        row.seq.get(),
                        skippr_lease::PublishMode::Republish,
                    )
                    .map_err(|err| DurableError::Io(err.to_string()))?;
                }
                ControlBody::OffsetHint { position, closed } => {
                    let CatalogKey::OffsetHint(hint) = key else {
                        continue;
                    };
                    publish_committed(
                        self.offsets.as_ref(),
                        &[CommittedOffset {
                            namespace: hint.namespace().to_string(),
                            partition: hint.partition().to_string(),
                            filesize: 0,
                            position,
                            closed: if closed { 1 } else { 0 },
                        }],
                        &[],
                        epoch.get(),
                        row.seq.get(),
                        skippr_lease::PublishMode::Republish,
                    )
                    .map_err(|err| DurableError::Io(err.to_string()))?;
                }
            }
        }
        self.offsets.flush().map_err(DurableError::Io)?;
        tracing::info!(
            segments,
            controls = control_count,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "WAL catalog republish complete"
        );
        Ok(())
    }

    pub fn hydrate_seq(&self) -> Result<CatalogSeq, DurableError> {
        match self
            .catalog
            .get(&CatalogKey::Seq)
            .map_err(|err| DurableError::Io(err.to_string()))?
        {
            Some(CatalogRow::Seq(row)) => Ok(row.max_seq),
            _ => Ok(CatalogSeq::ZERO),
        }
    }
}

fn flushing_commit(
    catalog: &dyn WalCatalogStore,
    txn: &CatalogTxn,
    fence: CatalogFence,
) -> Result<(), DurableError> {
    catalog
        .commit_durable(txn, fence)
        .map_err(|err| DurableError::Io(err.to_string()))
}

pub(crate) fn payload_bytes(
    location: &PayloadLocation,
    expected_sha: &[u8; 32],
) -> Result<Vec<u8>, DurableError> {
    let bytes = match location {
        PayloadLocation::Disk { path } => {
            std::fs::read(path).map_err(|err| DurableError::Io(err.to_string()))?
        }
        PayloadLocation::S3 { bucket, key } => {
            let bucket = bucket.clone();
            let key = key.clone();
            crate::buffer::wal_store::block_on_async(async move {
                crate::helpers::s3::get_object_bytes_for_bucket(&bucket, &key).await
            })
            .map_err(|err| DurableError::Io(err.to_string()))?
        }
    };
    let claimed = SegmentFile::verify_body_checksum(&bytes)
        .map_err(|err| DurableError::Io(err.to_string()))?;
    if &claimed != expected_sha {
        return Err(DurableError::ProtocolMismatch(
            "payload sha256 does not match catalog row".into(),
        ));
    }
    Ok(bytes)
}

fn delete_payload(location: &PayloadLocation) -> Result<(), DurableError> {
    match location {
        PayloadLocation::Disk { path } => {
            let _ = std::fs::remove_file(path);
            Ok(())
        }
        PayloadLocation::S3 { bucket, key } => {
            let bucket = bucket.clone();
            let key = key.clone();
            let _ = crate::buffer::wal_store::block_on_async(async move {
                crate::helpers::s3::delete_object(&bucket, &key)
                    .await
                    .map_err(|err| std::io::Error::other(err.to_string()))
            });
            Ok(())
        }
    }
}

fn meta_from_index_row(
    row: &SegmentRow,
    index: &SegmentIndexRow,
) -> crate::buffer::segment_file::SegmentFileMetadata {
    use crate::buffer::segment_file::{
        SegmentFileMetadata, SegmentPartMetaSummary, SegmentPartitionIndexEntry,
    };
    SegmentFileMetadata {
        created_at_secs: 0,
        total_bytes: row.payload_len,
        num_partitions: index.entries.len() as u32,
        offsets: HashMap::new(),
        index: index
            .entries
            .iter()
            .map(|entry| SegmentPartitionIndexEntry {
                key: PartitionKey {
                    sink_ref: entry.sink_ref.clone(),
                    namespace: entry.namespace.clone(),
                    partition: entry.partition.clone(),
                    time: entry.event_time,
                    schema_fingerprint: entry.schema_fingerprint.clone(),
                },
                bytes: entry.len,
                updated_at_secs: 0,
                slice_ordinal: entry.ordinal,
                part_meta_start: 0,
                part_meta_len: 0,
                part_meta_summary: match (entry.cdc_meta_hash, entry.row_count) {
                    (Some(canonical_hash), Some(row_count)) => SegmentPartMetaSummary::Cdc {
                        row_count,
                        canonical_hash,
                    },
                    (Some(canonical_hash), None) => SegmentPartMetaSummary::Cdc {
                        row_count: 0,
                        canonical_hash,
                    },
                    _ => SegmentPartMetaSummary::Append,
                },
                codec: entry.codec,
                start: entry.start,
                len: entry.len,
            })
            .collect(),
        body_sha256: row.payload_sha256,
    }
}

fn read_index_location(
    location: &PayloadLocation,
) -> Result<crate::buffer::segment_file::SegmentFileMetadata, DurableError> {
    match location {
        PayloadLocation::Disk { path } => SegmentFile::read_index_path(&PathBuf::from(path))
            .map_err(|err| DurableError::Io(err.to_string())),
        PayloadLocation::S3 { bucket, key } => {
            let bucket = bucket.clone();
            let key = key.clone();
            let bytes = crate::buffer::wal_store::block_on_async(async move {
                crate::helpers::s3::get_object_bytes_for_bucket(&bucket, &key).await
            })
            .map_err(|err| DurableError::Io(err.to_string()))?;
            SegmentFile::read_metadata_from_bytes(&bytes)
                .map_err(|err| DurableError::Io(err.to_string()))
        }
    }
}

fn slice_dir_entries(
    meta: &crate::buffer::segment_file::SegmentFileMetadata,
) -> Vec<SliceDirEntry> {
    meta.index
        .iter()
        .map(|idx| SliceDirEntry {
            ordinal: idx.slice_ordinal,
            namespace: idx.key.namespace.clone(),
            partition: idx.key.partition.clone(),
            event_time: idx.key.time,
            start: idx.start,
            len: idx.len,
            codec: idx.codec,
            row_count: idx.part_meta_summary.row_count(),
            sink_ref: idx.key.sink_ref.clone(),
            schema_fingerprint: idx.key.schema_fingerprint.clone(),
            cdc_meta_hash: idx.part_meta_summary.canonical_hash(),
        })
        .collect()
}

fn slice_index_from_dir(segment_id: &SegmentId, entry: &SliceDirEntry) -> SliceIndexEntry {
    SliceIndexEntry {
        segment_id: segment_id.clone(),
        ordinal: entry.ordinal,
        namespace: entry.namespace.clone(),
        partition: entry.partition.clone(),
        start: entry.start,
        len: entry.len,
        codec: entry.codec,
        version: 0,
    }
}

pub(crate) fn put_slice_index(
    txn: &mut CatalogTxn,
    id: &SegmentId,
    meta: &crate::buffer::segment_file::SegmentFileMetadata,
) -> Result<(), DurableError> {
    let entries = slice_dir_entries(meta);
    if entries.is_empty() {
        return Ok(());
    }
    txn.put(
        CatalogKey::Index(id.clone()),
        CatalogRow::Index(SegmentIndexRow {
            segment_id: id.clone(),
            version: 0,
            entries,
        }),
        PutCond::NotExists,
    )
    .map_err(|err| DurableError::Io(err.to_string()))?;
    Ok(())
}

fn payload_present(location: &PayloadLocation) -> bool {
    match location {
        PayloadLocation::Disk { path } => PathBuf::from(path).exists(),
        PayloadLocation::S3 { bucket, key } => crate::buffer::wal_store::block_on_async(async {
            match crate::helpers::s3::head_etag(bucket, key).await {
                Ok(Some(_)) => Ok(true),
                Ok(None) => Ok(false),
                Err(err) => Err(std::io::Error::other(err.to_string())),
            }
        })
        .unwrap_or(false),
    }
}

pub(crate) fn admit_payload(
    location: &PayloadLocation,
    expected_sha: &[u8; 32],
) -> Result<crate::buffer::segment_file::SegmentFileMetadata, DurableError> {
    match location {
        PayloadLocation::Disk { path } => {
            SegmentFile::admit_payload_path(&PathBuf::from(path), expected_sha)
                .map_err(|err| DurableError::Io(err.to_string()))
        }
        PayloadLocation::S3 { .. } => {
            let bytes = payload_bytes(location, expected_sha)?;
            let meta = SegmentFile::read_metadata_from_bytes(&bytes)
                .map_err(|err| DurableError::Io(err.to_string()))?;
            if meta.body_sha256 != *expected_sha {
                return Err(DurableError::ProtocolMismatch(
                    "FOOT digest disagrees with metadata scan".into(),
                ));
            }
            Ok(meta)
        }
    }
}

fn committed_offsets_from(
    offsets: &HashMap<OffsetKey, u64>,
    store: &dyn OffsetStore,
) -> Result<Vec<CommittedOffset>, DurableError> {
    let mut out = Vec::with_capacity(offsets.len());
    for (key, position) in offsets {
        let existing = store
            .get_offset(&key.namespace, &key.partition)
            .map_err(|err| DurableError::Io(err))?;
        let (filesize, closed) = match existing.as_deref() {
            Some(bytes) if bytes.len() == 24 => {
                let filesize = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
                (filesize, 1u64)
            }
            _ => (0, 1),
        };
        out.push(CommittedOffset {
            namespace: key.namespace.clone(),
            partition: key.partition.clone(),
            filesize,
            position: *position,
            closed,
        });
    }
    out.sort_by(|a, b| (&a.namespace, &a.partition).cmp(&(&b.namespace, &b.partition)));
    Ok(out)
}

fn checkpoints_from(
    updates: &HashMap<String, CheckpointEnvelope>,
) -> Result<Vec<CommittedCheckpoint>, DurableError> {
    let mut out = Vec::with_capacity(updates.len());
    for (key, envelope) in updates {
        out.push(CommittedCheckpoint {
            logical_key: key.clone(),
            envelope: bincode::serialize(envelope)
                .map_err(|err| DurableError::Io(err.to_string()))?,
        });
    }
    out.sort_by(|a, b| a.logical_key.cmp(&b.logical_key));
    Ok(out)
}

pub fn ensure_format_clean(
    catalog: &dyn WalCatalogStore,
    fence: CatalogFence,
) -> Result<(), DurableError> {
    match catalog
        .get(&CatalogKey::Format)
        .map_err(|err| DurableError::Io(err.to_string()))?
    {
        Some(CatalogRow::Format(row)) if row.phase == FormatPhase::Clean => Ok(()),
        Some(CatalogRow::Format(_)) => Ok(()),
        Some(_) => Err(DurableError::ProtocolMismatch(
            "wal#format is not a FormatRow".into(),
        )),
        None => {
            let mut txn = CatalogTxn::new();
            txn.put(
                CatalogKey::Format,
                CatalogRow::Format(FormatRow {
                    version: FORMAT_VERSION,
                    phase: FormatPhase::Clean,
                    row_version: 0,
                }),
                PutCond::NotExists,
            )
            .map_err(|err| DurableError::Io(err.to_string()))?;
            flushing_commit(catalog, &txn, fence)
        }
    }
}

pub fn catalog_fence_from_guard(guard: &LeaseGuard) -> Result<CatalogFence, DurableError> {
    if let Some(session) = guard.leased_session() {
        return Ok(CatalogFence {
            owner: session.owner,
            epoch: session.epoch,
        });
    }
    let epoch = guard
        .require_active_epoch()
        .map_err(|_| DurableError::Fenced)?;
    Ok(CatalogFence {
        owner: skippr_lease::NodeId::single_node(),
        epoch,
    })
}

pub fn memory_catalog_for_test(fence: CatalogFence) -> Arc<MemoryWalCatalog> {
    let catalog = MemoryWalCatalog::new();
    catalog.install_lease(fence.owner, fence.epoch);
    Arc::new(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::offset_store::SledOffsetStore;
    use skippr_lease::SystemClock;

    fn leased_handle(dir: &std::path::Path) -> Arc<PipelineWalHandle> {
        let key = PipelineKey::new("t", "w", "p").unwrap();
        let paths = PipelinePaths::legacy_disk(dir);
        let clock = Arc::new(SystemClock::new());
        let guard = LeaseGuard::single_node(key.clone(), clock);
        let fence = catalog_fence_from_guard(&guard).unwrap();
        let catalog = memory_catalog_for_test(fence);
        let db = sled::open(dir.join("offsets")).unwrap();
        let tree = db.open_tree("offsets").unwrap();
        let offsets: Arc<dyn OffsetStore> = Arc::new(SledOffsetStore::new(tree));
        PipelineWalHandle::new(key, paths, guard, catalog, offsets)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn persist_writes_catalog_row_without_commit_marker() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        let result = handle
            .persist_segment(
                "seg-1",
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
        let SegmentWriteLocation::Disk { path } = &result.location else {
            panic!("disk location");
        };
        assert!(path.exists());
        assert!(!path.with_extension("seg.commit").exists());
        let row = handle
            .catalog
            .get(&CatalogKey::Segment(SegmentId::new("seg-1").unwrap()))
            .unwrap()
            .unwrap();
        assert!(matches!(row, CatalogRow::Segment(_)));
        let index = handle
            .catalog
            .get(&CatalogKey::Index(SegmentId::new("seg-1").unwrap()))
            .unwrap();
        assert!(
            index.is_none(),
            "empty persist has no PART index, so no wal#idx#"
        );
        assert_eq!(handle.catalog_seq_seen().unwrap(), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn persist_writes_slice_index_for_parts() {
        use crate::buffer::segment_file::{PartitionKey, WalPayload};
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};

        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let mut batches = HashMap::new();
        batches.insert(
            PartitionKey {
                sink_ref: "data_sinks.iceberg".into(),
                namespace: "ns".into(),
                partition: "p0".into(),
                time: None,
                schema_fingerprint: "fp".into(),
            },
            WalPayload::Arrow(vec![arrow::array::RecordBatch::try_new(
                schema,
                vec![Arc::new(StringArray::from(vec!["evt-a"]))],
            )
            .unwrap()]),
        );
        handle
            .persist_segment(
                "seg-part",
                &HashMap::new(),
                &batches,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let index = handle
            .catalog
            .get(&CatalogKey::Index(SegmentId::new("seg-part").unwrap()))
            .unwrap();
        let Some(CatalogRow::Index(row)) = index else {
            panic!("persist must write wal#idx# SegmentIndexRow");
        };
        assert_eq!(row.entries.len(), 1);
        assert_eq!(row.entries[0].namespace, "ns");
        assert_eq!(row.entries[0].partition, "p0");
        assert!(row.entries[0].len > 0);
        assert!(handle
            .catalog
            .get(&CatalogKey::Slice {
                segment_id: SegmentId::new("seg-part").unwrap(),
                ordinal: 0,
            })
            .unwrap()
            .is_none());
    }

    #[test]
    fn select_live_ordinals_uses_catalog_slices_not_admit() {
        let src = include_str!("wal_handle.rs");
        let start = src
            .find("pub fn select_live_ordinals")
            .expect("select_live_ordinals");
        let body = src[start..]
            .split("pub fn slice_is_done")
            .next()
            .expect("body");
        assert!(
            !body.contains("admit_payload"),
            "query live ordinals must not hash/FOOT-admit payload"
        );
        assert!(
            body.contains("live_slices"),
            "live ordinals must come from catalog SliceIndexEntry"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn persist_retry_same_segment_id_skips_second_txn() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        handle
            .persist_segment(
                "seg-retry",
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        handle
            .persist_segment(
                "seg-retry",
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        assert_eq!(handle.catalog_seq_seen().unwrap(), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn persist_before_catalog_commit_failpoint() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        crate::cluster::failpoint::arm(crate::cluster::failpoint::BEFORE_CATALOG_COMMIT);
        let err = match handle
            .persist_segment(
                "seg-fp",
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
        {
            Ok(_) => panic!("failpoint should reject catalog commit"),
            Err(err) => err,
        };
        crate::cluster::failpoint::disarm();
        assert!(err.to_string().contains("before_catalog_commit"));
        assert!(handle
            .catalog
            .get(&CatalogKey::Segment(SegmentId::new("seg-fp").unwrap()))
            .unwrap()
            .is_none());
    }

    #[test]
    fn compaction_maps_slices_from_catalog_not_foot() {
        let src = include_str!("wal_handle.rs");
        let already = src
            .find("fn all_refs_already_done")
            .expect("all_refs_already_done");
        let already_body = src[already..]
            .split("fn resolve_ordinal")
            .next()
            .expect("already body");
        assert!(
            !already_body.contains("admit_payload"),
            "compaction identity is catalog directory ordinal"
        );
        let redo = src
            .find("async fn redo_acked_durable_batches")
            .expect("redo_acked_durable_batches");
        let redo_body = src[redo..]
            .split("pub async fn persist_checkpoint")
            .next()
            .unwrap_or(&src[redo..]);
        assert!(!redo_body.contains("admit_payload"));
        assert!(src.contains("fn resolve_ordinal"));
        assert!(!src.contains(concat!("fn catalog_slice_", "ordinal")));
    }

    #[test]
    fn catalog_commits_flush_before_payload_side_effects() {
        let src = include_str!("wal_handle.rs");
        assert!(src.contains("fn commit_durable"));
        assert!(
            !src.contains("self.catalog\n            .commit("),
            "handle mutations must commit through commit_durable so sled flushes before payload delete/PUT"
        );
    }

    fn one_arrow_part() -> (
        HashMap<crate::buffer::segment_file::PartitionKey, crate::buffer::segment_file::WalPayload>,
        crate::buffer::segment_file::PartitionKey,
    ) {
        use crate::buffer::segment_file::{PartitionKey, WalPayload};
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Utf8, true)]));
        let key = PartitionKey {
            sink_ref: "data_sinks.iceberg".into(),
            namespace: "ns".into(),
            partition: "p0".into(),
            time: None,
            schema_fingerprint: "fp".into(),
        };
        let mut batches = HashMap::new();
        batches.insert(
            key.clone(),
            WalPayload::Arrow(vec![arrow::array::RecordBatch::try_new(
                schema,
                vec![Arc::new(StringArray::from(vec!["evt-a"]))],
            )
            .unwrap()]),
        );
        (batches, key)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn hydrate_with_index_does_not_read_segf_headers() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        let (batches, _) = one_arrow_part();
        handle
            .persist_segment(
                "idx-seg",
                &HashMap::new(),
                &batches,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let before = crate::buffer::segment_file::SegmentFile::index_read_count();
        let row = match handle
            .catalog
            .get(&CatalogKey::Segment(SegmentId::new("idx-seg").unwrap()))
            .unwrap()
        {
            Some(CatalogRow::Segment(row)) => row,
            other => panic!("expected segment, got {other:?}"),
        };
        handle.load_or_build_index_meta(&row).unwrap();
        assert_eq!(
            crate::buffer::segment_file::SegmentFile::index_read_count(),
            before,
            "existing wal#idx# must not call read_index_path"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn missing_index_uses_read_index_path_once_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        let (batches, _) = one_arrow_part();
        handle
            .persist_segment(
                "rebuild-seg",
                &HashMap::new(),
                &batches,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let id = SegmentId::new("rebuild-seg").unwrap();
        let fence = handle.fence().unwrap();
        let mut txn = CatalogTxn::new();
        txn.delete(
            CatalogKey::Index(id.clone()),
            skippr_wal_catalog::DeleteCond::Exists,
        )
        .unwrap();
        handle.catalog.commit_durable(&txn, fence).unwrap();
        let row = match handle
            .catalog
            .get(&CatalogKey::Segment(id.clone()))
            .unwrap()
        {
            Some(CatalogRow::Segment(row)) => row,
            other => panic!("expected segment, got {other:?}"),
        };
        let before = crate::buffer::segment_file::SegmentFile::index_read_count();
        handle.load_or_build_index_meta(&row).unwrap();
        assert_eq!(
            crate::buffer::segment_file::SegmentFile::index_read_count(),
            before + 1
        );
        assert!(matches!(
            handle.catalog.get(&CatalogKey::Index(id)).unwrap(),
            Some(CatalogRow::Index(_))
        ));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn apply_acked_marks_bits_before_durable_flush() {
        use crate::buffer::compaction_transaction::{
            CompactionTransaction, SegmentSourceDescriptor, SinkWriteSemantics, WalPartRef,
        };
        use crate::plugins::source_contract::WritePolicy;
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        let (batches, key) = one_arrow_part();
        let result = handle
            .persist_segment(
                "acked-mem",
                &HashMap::new(),
                &batches,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let idx = &result.meta.index[0];
        let txn = CompactionTransaction::new(
            "out".into(),
            "ns".into(),
            "schema".into(),
            WritePolicy::Append,
            SinkWriteSemantics::AtLeastOnce,
            vec![WalPartRef {
                segment_id: "acked-mem".into(),
                source: SegmentSourceDescriptor::Disk {
                    path: handle.paths().segs.join("acked-mem.seg"),
                },
                start: idx.start,
                len: idx.len,
                key: key.clone(),
                cdc_meta_hash: None,
                ordinal: Some(idx.slice_ordinal),
            }],
            "out.parquet".into(),
        )
        .mark_acked();
        handle
            .put_compaction(
                &txn.id,
                skippr_wal_catalog::CompactionState::Acked,
                serde_json::to_vec(&txn).unwrap(),
            )
            .await
            .unwrap();
        let ids = handle.apply_acked_in_memory().unwrap();
        assert_eq!(ids, vec![txn.id.clone()]);
        assert!(handle
            .slice_is_done("acked-mem", idx.slice_ordinal)
            .unwrap());
        assert!(handle
            .catalog
            .get(&CatalogKey::Compaction(
                skippr_wal_catalog::CompactionId::new(&txn.id).unwrap()
            ))
            .unwrap()
            .is_some());
        assert!(handle.paths().segs.join("acked-mem.seg").exists());
        handle.redo_acked_durable_batches(&ids).await.unwrap();
        assert!(handle
            .catalog
            .get(&CatalogKey::Compaction(
                skippr_wal_catalog::CompactionId::new(&txn.id).unwrap()
            ))
            .unwrap()
            .is_none());
        assert!(!handle.paths().segs.join("acked-mem.seg").exists());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn durable_redo_batch_is_idempotent_after_partial_flush() {
        use crate::buffer::compaction_transaction::{
            CompactionTransaction, SegmentSourceDescriptor, SinkWriteSemantics, WalPartRef,
        };
        use crate::plugins::source_contract::WritePolicy;
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        let (batches, key) = one_arrow_part();
        let mut ids = Vec::new();
        for name in ["partial-a", "partial-b"] {
            let result = handle
                .persist_segment(
                    name,
                    &HashMap::new(),
                    &batches,
                    &HashMap::new(),
                    &HashMap::new(),
                    &HashMap::new(),
                )
                .await
                .unwrap();
            let idx = &result.meta.index[0];
            let txn = CompactionTransaction::new(
                "out".into(),
                name.into(),
                "schema".into(),
                WritePolicy::Append,
                SinkWriteSemantics::AtLeastOnce,
                vec![WalPartRef {
                    segment_id: name.into(),
                    source: SegmentSourceDescriptor::Disk {
                        path: handle.paths().segs.join(format!("{name}.seg")),
                    },
                    start: idx.start,
                    len: idx.len,
                    key: key.clone(),
                    cdc_meta_hash: None,
                    ordinal: Some(idx.slice_ordinal),
                }],
                format!("{name}.parquet"),
            )
            .mark_acked();
            ids.push(txn.id.clone());
            handle
                .put_compaction(
                    &txn.id,
                    skippr_wal_catalog::CompactionState::Acked,
                    serde_json::to_vec(&txn).unwrap(),
                )
                .await
                .unwrap();
        }
        handle.apply_acked_in_memory().unwrap();
        handle.redo_acked_durable_batches(&ids[..1]).await.unwrap();
        assert!(handle
            .catalog
            .get(&CatalogKey::Compaction(
                skippr_wal_catalog::CompactionId::new(&ids[1]).unwrap()
            ))
            .unwrap()
            .is_some());
        assert!(handle.paths().segs.join("partial-b.seg").exists());
        handle.redo_acked_durable_batches(&ids).await.unwrap();
        assert!(handle
            .catalog
            .get(&CatalogKey::Compaction(
                skippr_wal_catalog::CompactionId::new(&ids[0]).unwrap()
            ))
            .unwrap()
            .is_none());
        assert!(handle
            .catalog
            .get(&CatalogKey::Compaction(
                skippr_wal_catalog::CompactionId::new(&ids[1]).unwrap()
            ))
            .unwrap()
            .is_none());
        assert!(!handle.paths().segs.join("partial-b.seg").exists());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn legacy_wal_part_ref_without_ordinal_resolves_from_headers() {
        use crate::buffer::compaction_transaction::{
            CompactionTransaction, SegmentSourceDescriptor, SinkWriteSemantics, WalPartRef,
        };
        use crate::plugins::source_contract::WritePolicy;
        let dir = tempfile::tempdir().unwrap();
        let handle = leased_handle(dir.path());
        let (batches, key) = one_arrow_part();
        let result = handle
            .persist_segment(
                "legacy-ord",
                &HashMap::new(),
                &batches,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let idx = &result.meta.index[0];
        let body = serde_json::to_vec(
            &CompactionTransaction::new(
                "out".into(),
                "ns".into(),
                "schema".into(),
                WritePolicy::Append,
                SinkWriteSemantics::AtLeastOnce,
                vec![WalPartRef {
                    segment_id: "legacy-ord".into(),
                    source: SegmentSourceDescriptor::Disk {
                        path: handle.paths().segs.join("legacy-ord.seg"),
                    },
                    start: idx.start,
                    len: idx.len,
                    key,
                    cdc_meta_hash: None,
                    ordinal: None,
                }],
                "out.parquet".into(),
            )
            .mark_acked(),
        )
        .unwrap();
        let parsed: crate::buffer::compaction_transaction::CompactionTransaction =
            serde_json::from_slice(&body).unwrap();
        assert!(parsed.refs[0].ordinal.is_none());
        handle
            .put_compaction(&parsed.id, skippr_wal_catalog::CompactionState::Acked, body)
            .await
            .unwrap();
        let ids = handle.apply_acked_in_memory().unwrap();
        assert!(handle
            .slice_is_done("legacy-ord", idx.slice_ordinal)
            .unwrap());
        handle.redo_acked_durable_batches(&ids).await.unwrap();
        assert!(handle
            .catalog
            .get(&CatalogKey::Compaction(
                skippr_wal_catalog::CompactionId::new(&parsed.id).unwrap()
            ))
            .unwrap()
            .is_none());
    }

    #[test]
    fn durable_redo_uses_batches_not_two_flushes_per_row() {
        let src = include_str!("wal_handle.rs");
        assert!(src.contains("const REDO_ACKED_BATCH: usize = 32"));
        let redo = src
            .find("async fn redo_acked_durable_batches")
            .expect("durable batches");
        let body = src[redo..]
            .split("async fn flush_acked_batch")
            .next()
            .expect("batch loop");
        assert!(body.contains("REDO_ACKED_BATCH"));
        assert!(!body.contains("complete_slices_and_reclaim"));
        assert!(!body.contains("delete_compaction"));
    }
}
