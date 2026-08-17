use crate::buffer::compaction_transaction::{
    CompactionTransaction, CompactionTransactionState, WalPartRef,
};
use crate::buffer::segment_file::SegmentPartitionIndexEntry;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum CompactionKind {
    Append,
    Cdc,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CompactionGroupKey {
    pub sink_ref: String,
    pub namespace: String,
    pub partition: String,
    pub time: Option<i64>,
    pub schema_fingerprint: String,
    pub kind: CompactionKind,
}

impl CompactionGroupKey {
    fn lane(&self) -> CompactionLaneKey {
        CompactionLaneKey {
            sink_ref: self.sink_ref.clone(),
            namespace: self.namespace.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CompactionSliceId {
    pub segment_id: String,
    pub source_id: String,
    pub start: u64,
    pub len: u64,
}

impl CompactionSliceId {
    pub(crate) fn from_wal_ref(wal_ref: &WalPartRef) -> Self {
        Self {
            segment_id: wal_ref.segment_id.clone(),
            source_id: wal_ref.source.stable_id(),
            start: wal_ref.start,
            len: wal_ref.len,
        }
    }
}

#[derive(Clone)]
pub(crate) struct IndexedCompactionSlice {
    pub id: CompactionSliceId,
    pub group_key: CompactionGroupKey,
    pub index_entry: SegmentPartitionIndexEntry,
    pub wal_ref: WalPartRef,
}

impl IndexedCompactionSlice {
    fn segment_key(&self) -> SegmentKey {
        SegmentKey {
            segment_id: self.id.segment_id.clone(),
            source_id: self.id.source_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct SegmentKey {
    segment_id: String,
    source_id: String,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CompactionLaneKey {
    pub(crate) sink_ref: String,
    pub(crate) namespace: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct QueueSliceKey {
    updated_at_secs: u64,
    id: CompactionSliceId,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct QueueHead {
    updated_at_secs: u64,
    segment_remaining: usize,
    segment: SegmentKey,
    id: CompactionSliceId,
}

#[derive(Default)]
struct SegmentReadyQueue {
    slices: BTreeSet<QueueSliceKey>,
    head: Option<QueueHead>,
}

/// A closure-aware queue. The oldest slice wins; equal-age slices prefer the
/// segment with the fewest remaining uncompleted slices.
#[derive(Default)]
pub(crate) struct ReadyQueue {
    segments: BTreeMap<SegmentKey, SegmentReadyQueue>,
    heads: BTreeSet<QueueHead>,
    len: usize,
}

impl ReadyQueue {
    fn insert(&mut self, slice: &IndexedCompactionSlice, segment_remaining: usize) {
        let segment = slice.segment_key();
        self.remove_head(&segment);
        let inserted = self
            .segments
            .entry(segment.clone())
            .or_default()
            .slices
            .insert(QueueSliceKey {
                updated_at_secs: slice.index_entry.updated_at_secs,
                id: slice.id.clone(),
            });
        if inserted {
            self.len = self.len.saturating_add(1);
        }
        self.install_head(&segment, segment_remaining);
    }

    fn remove(&mut self, slice: &IndexedCompactionSlice, segment_remaining: usize) -> bool {
        let segment = slice.segment_key();
        self.remove_head(&segment);
        let key = QueueSliceKey {
            updated_at_secs: slice.index_entry.updated_at_secs,
            id: slice.id.clone(),
        };
        let removed = self
            .segments
            .get_mut(&segment)
            .map(|queue| queue.slices.remove(&key))
            .unwrap_or(false);
        if removed {
            self.len = self.len.saturating_sub(1);
        }
        let empty = self
            .segments
            .get(&segment)
            .map(|queue| queue.slices.is_empty())
            .unwrap_or(false);
        if empty {
            self.segments.remove(&segment);
        } else {
            self.install_head(&segment, segment_remaining);
        }
        removed
    }

    fn update_segment_remaining(&mut self, segment: &SegmentKey, remaining: usize) {
        if !self.segments.contains_key(segment) {
            return;
        }
        self.remove_head(segment);
        self.install_head(segment, remaining);
    }

    fn peek(&self) -> Option<&QueueHead> {
        self.heads.first()
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn remove_head(&mut self, segment: &SegmentKey) {
        if let Some(old) = self
            .segments
            .get_mut(segment)
            .and_then(|queue| queue.head.take())
        {
            self.heads.remove(&old);
        }
    }

    fn install_head(&mut self, segment: &SegmentKey, segment_remaining: usize) {
        let Some(queue) = self.segments.get_mut(segment) else {
            return;
        };
        let Some(first) = queue.slices.first() else {
            return;
        };
        let head = QueueHead {
            updated_at_secs: first.updated_at_secs,
            segment_remaining,
            segment: segment.clone(),
            id: first.id.clone(),
        };
        self.heads.insert(head.clone());
        queue.head = Some(head);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SliceState {
    Waiting,
    Ready,
    Reserved,
    Fenced,
    DeferredOversize,
}

struct SliceRecord {
    slice: IndexedCompactionSlice,
    state: SliceState,
    wait_generation: u64,
}

#[derive(Default)]
struct SegmentState {
    remaining: usize,
    ids: BTreeSet<CompactionSliceId>,
    groups: BTreeSet<CompactionGroupKey>,
    lanes: BTreeSet<CompactionLaneKey>,
}

#[derive(Default)]
struct LaneState {
    queue: ReadyQueue,
    last_served: u64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct WaitingEntry {
    eligible_at_secs: u64,
    id: CompactionSliceId,
    generation: u64,
}

struct ManifestRecord {
    txn: CompactionTransaction,
    reserved: bool,
}

#[derive(Default)]
struct ManifestIndex {
    records: BTreeMap<String, ManifestRecord>,
    fences: HashMap<CompactionSliceId, BTreeSet<String>>,
    lane_last_served: BTreeMap<CompactionLaneKey, u64>,
    service_ticket: u64,
}

impl ManifestIndex {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn is_fenced(&self, id: &CompactionSliceId) -> bool {
        self.fences.contains_key(id)
    }

    fn install(&mut self, txns: Vec<CompactionTransaction>) {
        self.clear();
        for txn in txns {
            self.upsert(txn, false);
        }
    }

    fn upsert(&mut self, txn: CompactionTransaction, reserve_if_new: bool) {
        let id = txn.id.clone();
        let reserved = self
            .records
            .get(&id)
            .map(|record| record.reserved)
            .unwrap_or(reserve_if_new);
        if let Some(old) = self.records.remove(&id) {
            self.remove_fences(&id, &old.txn.refs);
        }
        if matches!(txn.state, CompactionTransactionState::Tombstoned) {
            return;
        }
        self.add_fences(&id, &txn.refs);
        self.records.insert(id, ManifestRecord { txn, reserved });
    }

    fn remove(&mut self, id: &str) -> Vec<CompactionSliceId> {
        let Some(record) = self.records.remove(id) else {
            return Vec::new();
        };
        let refs = record
            .txn
            .refs
            .iter()
            .map(CompactionSliceId::from_wal_ref)
            .collect::<Vec<_>>();
        self.remove_fences(id, &record.txn.refs);
        refs
    }

    fn release(&mut self, id: &str) {
        if let Some(record) = self.records.get_mut(id) {
            record.reserved = false;
        }
    }

    fn reserve_ready(
        &mut self,
        limit: usize,
        per_sink_limit: usize,
        now_secs: u64,
        sent_stale_secs: u64,
        active_by_lane: &HashMap<CompactionLaneKey, usize>,
        blocked_lanes: &HashSet<CompactionLaneKey>,
    ) -> Vec<ReservedManifest> {
        let mut out = Vec::with_capacity(limit);
        let mut scheduled_by_sink = active_counts_by_sink(active_by_lane);
        while out.len() < limit {
            let candidate = self
                .records
                .iter()
                .filter(|(_, record)| {
                    let lane = CompactionLaneKey {
                        sink_ref: record.txn.sink_ref.clone(),
                        namespace: record.txn.namespace.clone(),
                    };
                    !record.reserved
                        && manifest_is_ready(&record.txn, now_secs, sent_stale_secs)
                        && !blocked_lanes.contains(&lane)
                        && scheduled_by_sink
                            .get(&record.txn.sink_ref)
                            .copied()
                            .unwrap_or(0)
                            < per_sink_limit
                })
                .map(|(id, record)| {
                    let lane = CompactionLaneKey {
                        sink_ref: record.txn.sink_ref.clone(),
                        namespace: record.txn.namespace.clone(),
                    };
                    let last_served = self.lane_last_served.get(&lane).copied().unwrap_or(0);
                    (
                        last_served,
                        record.txn.created_at_secs,
                        record.txn.updated_at_secs,
                        lane,
                        id.clone(),
                    )
                })
                .min();
            let Some((_, _, _, lane, id)) = candidate else {
                break;
            };
            let Some(record) = self.records.get_mut(&id) else {
                continue;
            };
            let retried_stale = matches!(record.txn.state, CompactionTransactionState::Sent);
            let stale_age_secs = if retried_stale {
                now_secs.saturating_sub(record.txn.updated_at_secs)
            } else {
                0
            };
            if retried_stale {
                record.txn.state = CompactionTransactionState::Pending;
                record.txn.updated_at_secs = now_secs;
            }
            record.reserved = true;
            *scheduled_by_sink
                .entry(record.txn.sink_ref.clone())
                .or_default() += 1;
            self.service_ticket = self.service_ticket.saturating_add(1);
            self.lane_last_served.insert(lane, self.service_ticket);
            out.push(ReservedManifest {
                txn: record.txn.clone(),
                retried_stale,
                stale_age_secs,
            });
        }
        out
    }

    fn ready_ref_count(&self, now_secs: u64, sent_stale_secs: u64) -> usize {
        self.records
            .values()
            .filter(|record| {
                !record.reserved && manifest_is_ready(&record.txn, now_secs, sent_stale_secs)
            })
            .map(|record| record.txn.refs.len())
            .sum()
    }

    fn add_fences(&mut self, txn_id: &str, refs: &[WalPartRef]) {
        for wal_ref in refs {
            self.fences
                .entry(CompactionSliceId::from_wal_ref(wal_ref))
                .or_default()
                .insert(txn_id.to_string());
        }
    }

    fn remove_fences(&mut self, txn_id: &str, refs: &[WalPartRef]) {
        for wal_ref in refs {
            let key = CompactionSliceId::from_wal_ref(wal_ref);
            let remove_key = if let Some(ids) = self.fences.get_mut(&key) {
                ids.remove(txn_id);
                ids.is_empty()
            } else {
                false
            };
            if remove_key {
                self.fences.remove(&key);
            }
        }
    }
}

fn active_counts_by_sink(
    active_by_lane: &HashMap<CompactionLaneKey, usize>,
) -> HashMap<String, usize> {
    let mut by_sink = HashMap::new();
    for (lane, active) in active_by_lane {
        *by_sink.entry(lane.sink_ref.clone()).or_default() += *active;
    }
    by_sink
}

fn manifest_is_ready(txn: &CompactionTransaction, now_secs: u64, sent_stale_secs: u64) -> bool {
    match txn.state {
        CompactionTransactionState::Pending | CompactionTransactionState::Acked => true,
        CompactionTransactionState::Sent => {
            now_secs.saturating_sub(txn.updated_at_secs) >= sent_stale_secs
        }
        CompactionTransactionState::Tombstoned => false,
    }
}

fn wall_clock_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) struct ReservedManifest {
    pub txn: CompactionTransaction,
    pub retried_stale: bool,
    pub stale_age_secs: u64,
}

pub(crate) struct PlannedCompactionGroup {
    pub key: CompactionGroupKey,
    pub slices: Vec<IndexedCompactionSlice>,
}

#[derive(Default)]
pub(crate) struct CompactionPlan {
    pub groups: Vec<PlannedCompactionGroup>,
    pub segments_examined: u64,
    pub slices_examined: u64,
    pub ready_queue_depth: usize,
    pub oversized_slices_deferred: usize,
}

/// Process-local WAL compaction index. Segment registration is the only path
/// that walks segment metadata; planning consumes queue heads incrementally.
#[derive(Default)]
pub(crate) struct CompactionIndex {
    slices: HashMap<CompactionSliceId, SliceRecord>,
    segments: BTreeMap<SegmentKey, SegmentState>,
    groups: BTreeMap<CompactionGroupKey, ReadyQueue>,
    lanes: BTreeMap<CompactionLaneKey, LaneState>,
    waiting: BTreeSet<CompactionSliceId>,
    waiting_by_age: BinaryHeap<Reverse<WaitingEntry>>,
    deferred_oversize: BTreeSet<CompactionSliceId>,
    force_only_ready: BTreeSet<CompactionSliceId>,
    ready_count: usize,
    ready_group_count: usize,
    byte_threshold: Option<u64>,
    time_threshold_secs: Option<u64>,
    target_bytes: Option<u64>,
    service_ticket: u64,
    manifests: ManifestIndex,
    manifests_loaded: bool,
}

impl CompactionIndex {
    pub(crate) fn clear_for_recovery(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    pub(crate) fn clear_all(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn manifests_loaded(&self) -> bool {
        self.manifests_loaded
    }

    pub(crate) fn install_manifests(&mut self, txns: Vec<CompactionTransaction>) {
        self.manifests.install(txns);
        self.manifests_loaded = true;
        let ids = self.slices.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            if self.manifests.is_fenced(&id) {
                self.set_fenced(&id);
            }
        }
    }

    pub(crate) fn register_segment(
        &mut self,
        segment_id: &str,
        slices: Vec<IndexedCompactionSlice>,
        byte_threshold: u64,
        time_threshold_secs: u64,
        now_secs: u64,
    ) -> usize {
        self.configure_thresholds(byte_threshold, time_threshold_secs, now_secs);
        self.remove_segment(segment_id);
        if slices.is_empty() {
            return 0;
        }

        let segment_key = slices[0].segment_key();
        let mut segment = SegmentState::default();
        for slice in slices {
            if self.slices.contains_key(&slice.id) {
                continue;
            }
            segment.remaining = segment.remaining.saturating_add(1);
            segment.ids.insert(slice.id.clone());
            segment.groups.insert(slice.group_key.clone());
            segment.lanes.insert(slice.group_key.lane());
            let state = if self.manifests.is_fenced(&slice.id) {
                SliceState::Fenced
            } else {
                SliceState::Waiting
            };
            self.slices.insert(
                slice.id.clone(),
                SliceRecord {
                    slice,
                    state,
                    wait_generation: 0,
                },
            );
        }
        let registered = segment.remaining;
        self.segments.insert(segment_key.clone(), segment);
        let ids = self
            .segments
            .get(&segment_key)
            .map(|segment| segment.ids.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for id in ids {
            if matches!(
                self.slices.get(&id).map(|record| record.state),
                Some(SliceState::Waiting)
            ) {
                self.classify_available(&id, now_secs);
            }
        }
        registered
    }

    pub(crate) fn remove_segment(&mut self, segment_id: &str) {
        let ids = self
            .segments
            .iter()
            .filter(|(key, _)| key.segment_id == segment_id)
            .flat_map(|(_, state)| state.ids.iter().cloned())
            .collect::<Vec<_>>();
        for id in ids {
            self.remove_slice(&id);
        }
    }

    pub(crate) fn drop_segments_not_in(&mut self, live: &HashSet<String>) {
        let stale: HashSet<String> = self
            .segments
            .keys()
            .map(|key| key.segment_id.clone())
            .filter(|id| !live.contains(id))
            .collect();
        for id in stale {
            self.remove_segment(&id);
        }
    }

    pub(crate) fn complete_ordinals(&mut self, segment_id: &str, ordinals: &[u32]) -> usize {
        let wanted: HashSet<u32> = ordinals.iter().copied().collect();
        if wanted.is_empty() {
            return 0;
        }
        let ids: Vec<CompactionSliceId> = self
            .slices
            .values()
            .filter(|record| {
                record.slice.id.segment_id == segment_id
                    && wanted.contains(&record.slice.index_entry.slice_ordinal)
            })
            .map(|record| record.slice.id.clone())
            .collect();
        ids.iter().filter(|id| self.remove_slice(id)).count()
    }

    #[cfg(test)]
    pub(crate) fn live_compaction_ids(&self) -> Vec<String> {
        self.manifests.records.keys().cloned().collect()
    }

    pub(crate) fn complete_refs(&mut self, refs: &[WalPartRef]) -> usize {
        refs.iter()
            .filter(|wal_ref| self.remove_slice(&CompactionSliceId::from_wal_ref(wal_ref)))
            .count()
    }

    pub(crate) fn release_refs(&mut self, refs: &[WalPartRef], now_secs: u64) {
        for wal_ref in refs {
            let id = CompactionSliceId::from_wal_ref(wal_ref);
            if !matches!(
                self.slices.get(&id).map(|record| record.state),
                Some(SliceState::Reserved)
            ) {
                continue;
            }
            if self.manifests.is_fenced(&id) {
                if let Some(record) = self.slices.get_mut(&id) {
                    record.state = SliceState::Fenced;
                }
            } else {
                self.classify_available(&id, now_secs);
            }
        }
    }

    pub(crate) fn upsert_manifest(&mut self, txn: CompactionTransaction) {
        let old_refs = self
            .manifests
            .records
            .get(&txn.id)
            .map(|record| record.txn.refs.clone())
            .unwrap_or_default();
        let reserve_if_new = txn.refs.iter().any(|wal_ref| {
            matches!(
                self.slices
                    .get(&CompactionSliceId::from_wal_ref(wal_ref))
                    .map(|record| record.state),
                Some(SliceState::Reserved)
            )
        });
        self.manifests.upsert(txn.clone(), reserve_if_new);
        let affected = old_refs
            .iter()
            .chain(txn.refs.iter())
            .map(CompactionSliceId::from_wal_ref)
            .collect::<BTreeSet<_>>();
        for id in affected {
            if self.manifests.is_fenced(&id) {
                if !matches!(
                    self.slices.get(&id).map(|record| record.state),
                    Some(SliceState::Reserved)
                ) {
                    self.set_fenced(&id);
                }
            } else if matches!(
                self.slices.get(&id).map(|record| record.state),
                Some(SliceState::Fenced)
            ) {
                self.classify_available(&id, wall_clock_secs());
            }
        }
    }

    pub(crate) fn remove_manifest(&mut self, id: &str, now_secs: u64) {
        let affected = self.manifests.remove(id);
        for slice_id in affected {
            if !self.manifests.is_fenced(&slice_id)
                && matches!(
                    self.slices.get(&slice_id).map(|record| record.state),
                    Some(SliceState::Fenced)
                )
            {
                self.classify_available(&slice_id, now_secs);
            }
        }
    }

    pub(crate) fn release_manifest(&mut self, id: &str) {
        self.manifests.release(id);
    }

    pub(crate) fn reserve_ready_manifests(
        &mut self,
        limit: usize,
        per_sink_limit: usize,
        now_secs: u64,
        sent_stale_secs: u64,
        active_by_lane: &HashMap<CompactionLaneKey, usize>,
        blocked_lanes: &HashSet<CompactionLaneKey>,
    ) -> Vec<ReservedManifest> {
        self.manifests.reserve_ready(
            limit,
            per_sink_limit,
            now_secs,
            sent_stale_secs,
            active_by_lane,
            blocked_lanes,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn plan_ready(
        &mut self,
        limit: usize,
        force: bool,
        now_secs: u64,
        byte_threshold: u64,
        time_threshold_secs: u64,
        target_bytes: u64,
        max_parts: usize,
        per_sink_limit: usize,
        active_by_lane: HashMap<CompactionLaneKey, usize>,
        blocked_lanes: HashSet<CompactionLaneKey>,
    ) -> CompactionPlan {
        self.configure_thresholds(byte_threshold, time_threshold_secs, now_secs);
        self.requeue_deferred_if_target_changed(target_bytes, now_secs);
        if force {
            self.promote_all_waiting(now_secs);
        } else {
            self.demote_force_only(now_secs);
            self.promote_due(now_secs);
        }

        let mut plan = CompactionPlan::default();
        let mut examined_segments = HashSet::<SegmentKey>::new();
        let mut scheduled_by_sink = active_counts_by_sink(&active_by_lane);
        while plan.groups.len() < limit {
            let lane_candidate = self
                .lanes
                .iter()
                .filter(|(lane, state)| {
                    !state.queue.is_empty()
                        && !blocked_lanes.contains(*lane)
                        && scheduled_by_sink.get(&lane.sink_ref).copied().unwrap_or(0)
                            < per_sink_limit
                })
                .filter_map(|(lane, state)| {
                    state.queue.peek().map(|head| {
                        (
                            state.last_served,
                            head.updated_at_secs,
                            head.segment_remaining,
                            lane.clone(),
                            head.id.clone(),
                        )
                    })
                })
                .min();
            let Some((_, _, _, lane, first_id)) = lane_candidate else {
                break;
            };
            let Some(group_key) = self
                .slices
                .get(&first_id)
                .map(|record| record.slice.group_key.clone())
            else {
                break;
            };

            let mut selected = Vec::new();
            let mut selected_bytes = 0u64;
            while selected.len() < max_parts {
                let next_id = self
                    .groups
                    .get(&group_key)
                    .and_then(ReadyQueue::peek)
                    .map(|head| head.id.clone());
                let Some(next_id) = next_id else {
                    break;
                };
                plan.slices_examined = plan.slices_examined.saturating_add(1);
                if let Some(segment) = self
                    .slices
                    .get(&next_id)
                    .map(|record| record.slice.segment_key())
                {
                    examined_segments.insert(segment);
                }
                let Some(bytes) = self
                    .slices
                    .get(&next_id)
                    .map(|record| record.slice.index_entry.bytes)
                else {
                    break;
                };
                if bytes > target_bytes && selected.is_empty() {
                    self.defer_oversize(&next_id);
                    plan.oversized_slices_deferred =
                        plan.oversized_slices_deferred.saturating_add(1);
                    break;
                }
                if selected_bytes.saturating_add(bytes) > target_bytes {
                    break;
                }
                let Some(slice) = self.reserve_ready_slice(&next_id) else {
                    break;
                };
                selected_bytes = selected_bytes.saturating_add(bytes);
                selected.push(slice);
            }
            if selected.is_empty() {
                continue;
            }

            self.service_ticket = self.service_ticket.saturating_add(1);
            if let Some(state) = self.lanes.get_mut(&lane) {
                state.last_served = self.service_ticket;
            }
            *scheduled_by_sink
                .entry(group_key.sink_ref.clone())
                .or_default() += 1;
            plan.groups.push(PlannedCompactionGroup {
                key: group_key,
                slices: selected,
            });
        }
        plan.segments_examined = examined_segments.len() as u64;
        plan.ready_queue_depth = self.ready_group_count;
        plan
    }

    pub(crate) fn indexed_slice_count(&self) -> usize {
        self.slices.len()
    }

    pub(crate) fn reclaimable_slice_count(
        &mut self,
        force: bool,
        now_secs: u64,
        sent_stale_secs: u64,
        limit: usize,
    ) -> usize {
        if !force {
            self.promote_due(now_secs);
        }
        self.ready_count
            .saturating_add(if force { self.waiting.len() } else { 0 })
            .saturating_add(self.manifests.ready_ref_count(now_secs, sent_stale_secs))
            .min(limit)
    }

    pub(crate) fn ready_queue_depth(&self) -> usize {
        self.ready_group_count
    }

    fn configure_thresholds(
        &mut self,
        byte_threshold: u64,
        time_threshold_secs: u64,
        now_secs: u64,
    ) {
        if self.byte_threshold == Some(byte_threshold)
            && self.time_threshold_secs == Some(time_threshold_secs)
        {
            return;
        }
        self.byte_threshold = Some(byte_threshold);
        self.time_threshold_secs = Some(time_threshold_secs);
        self.rebuild_available_queues(now_secs);
    }

    fn rebuild_available_queues(&mut self, now_secs: u64) {
        self.groups.clear();
        self.lanes.clear();
        self.waiting.clear();
        self.waiting_by_age.clear();
        self.force_only_ready.clear();
        self.ready_count = 0;
        self.ready_group_count = 0;
        let ids = self
            .slices
            .iter()
            .filter(|(_, record)| matches!(record.state, SliceState::Waiting | SliceState::Ready))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(record) = self.slices.get_mut(&id) {
                record.state = SliceState::Waiting;
            }
            self.classify_available(&id, now_secs);
        }
    }

    fn classify_available(&mut self, id: &CompactionSliceId, now_secs: u64) {
        if self.manifests.is_fenced(id) {
            self.set_fenced(id);
            return;
        }
        let Some(record) = self.slices.get(id) else {
            return;
        };
        let bytes_ready = record.slice.index_entry.bytes >= self.byte_threshold.unwrap_or(u64::MAX);
        let late_ready = now_secs.saturating_sub(record.slice.index_entry.updated_at_secs)
            >= self.time_threshold_secs.unwrap_or(u64::MAX);
        if bytes_ready || late_ready {
            self.set_ready(id);
        } else {
            self.set_waiting(id);
        }
    }

    fn set_waiting(&mut self, id: &CompactionSliceId) {
        self.force_only_ready.remove(id);
        let Some(record) = self.slices.get_mut(id) else {
            return;
        };
        record.state = SliceState::Waiting;
        record.wait_generation = record.wait_generation.saturating_add(1);
        let generation = record.wait_generation;
        let eligible_at_secs = record
            .slice
            .index_entry
            .updated_at_secs
            .saturating_add(self.time_threshold_secs.unwrap_or(u64::MAX));
        self.waiting.insert(id.clone());
        self.waiting_by_age.push(Reverse(WaitingEntry {
            eligible_at_secs,
            id: id.clone(),
            generation,
        }));
    }

    fn set_ready(&mut self, id: &CompactionSliceId) {
        if matches!(
            self.slices.get(id).map(|record| record.state),
            Some(SliceState::Ready)
        ) {
            return;
        }
        self.waiting.remove(id);
        self.deferred_oversize.remove(id);
        self.force_only_ready.remove(id);
        let Some(slice) = self.slices.get(id).map(|record| record.slice.clone()) else {
            return;
        };
        let remaining = self
            .segments
            .get(&slice.segment_key())
            .map(|segment| segment.remaining)
            .unwrap_or(0);
        let group_queue = self.groups.entry(slice.group_key.clone()).or_default();
        let group_was_empty = group_queue.is_empty();
        group_queue.insert(&slice, remaining);
        if group_was_empty && !group_queue.is_empty() {
            self.ready_group_count = self.ready_group_count.saturating_add(1);
        }
        self.lanes
            .entry(slice.group_key.lane())
            .or_default()
            .queue
            .insert(&slice, remaining);
        if let Some(record) = self.slices.get_mut(id) {
            record.state = SliceState::Ready;
        }
        self.ready_count = self.ready_count.saturating_add(1);
    }

    fn set_fenced(&mut self, id: &CompactionSliceId) {
        self.remove_from_current_queue(id);
        self.force_only_ready.remove(id);
        if let Some(record) = self.slices.get_mut(id) {
            record.state = SliceState::Fenced;
        }
    }

    fn defer_oversize(&mut self, id: &CompactionSliceId) {
        self.remove_from_current_queue(id);
        self.force_only_ready.remove(id);
        if let Some(record) = self.slices.get_mut(id) {
            record.state = SliceState::DeferredOversize;
        }
        self.deferred_oversize.insert(id.clone());
    }

    fn reserve_ready_slice(&mut self, id: &CompactionSliceId) -> Option<IndexedCompactionSlice> {
        if !matches!(
            self.slices.get(id).map(|record| record.state),
            Some(SliceState::Ready)
        ) {
            return None;
        }
        self.remove_from_current_queue(id);
        self.force_only_ready.remove(id);
        let record = self.slices.get_mut(id)?;
        record.state = SliceState::Reserved;
        Some(record.slice.clone())
    }

    fn remove_from_current_queue(&mut self, id: &CompactionSliceId) {
        self.waiting.remove(id);
        self.deferred_oversize.remove(id);
        let Some((slice, state)) = self
            .slices
            .get(id)
            .map(|record| (record.slice.clone(), record.state))
        else {
            return;
        };
        if state != SliceState::Ready {
            return;
        }
        let remaining = self
            .segments
            .get(&slice.segment_key())
            .map(|segment| segment.remaining)
            .unwrap_or(0);
        if let Some(queue) = self.groups.get_mut(&slice.group_key) {
            let was_nonempty = !queue.is_empty();
            queue.remove(&slice, remaining);
            if was_nonempty && queue.is_empty() {
                self.ready_group_count = self.ready_group_count.saturating_sub(1);
            }
        }
        if let Some(lane) = self.lanes.get_mut(&slice.group_key.lane()) {
            lane.queue.remove(&slice, remaining);
        }
        self.ready_count = self.ready_count.saturating_sub(1);
    }

    fn promote_due(&mut self, now_secs: u64) {
        loop {
            let Some(Reverse(entry)) = self.waiting_by_age.peek() else {
                break;
            };
            if entry.eligible_at_secs > now_secs {
                break;
            }
            let entry = self.waiting_by_age.pop().map(|Reverse(entry)| entry);
            let Some(entry) = entry else {
                break;
            };
            let valid = self
                .slices
                .get(&entry.id)
                .map(|record| {
                    record.state == SliceState::Waiting
                        && record.wait_generation == entry.generation
                })
                .unwrap_or(false);
            if valid {
                self.set_ready(&entry.id);
            }
        }
    }

    fn promote_all_waiting(&mut self, now_secs: u64) {
        let ids = self.waiting.iter().cloned().collect::<Vec<_>>();
        for id in ids {
            let normal_eligible = self.normal_eligible(&id, now_secs);
            self.set_ready(&id);
            if !normal_eligible {
                self.force_only_ready.insert(id);
            }
        }
    }

    fn demote_force_only(&mut self, now_secs: u64) {
        let ids = self.force_only_ready.iter().cloned().collect::<Vec<_>>();
        for id in ids {
            self.remove_from_current_queue(&id);
            self.force_only_ready.remove(&id);
            if let Some(record) = self.slices.get_mut(&id) {
                record.state = SliceState::Waiting;
            }
            self.classify_available(&id, now_secs);
        }
    }

    fn normal_eligible(&self, id: &CompactionSliceId, now_secs: u64) -> bool {
        self.slices
            .get(id)
            .map(|record| {
                record.slice.index_entry.bytes >= self.byte_threshold.unwrap_or(u64::MAX)
                    || now_secs.saturating_sub(record.slice.index_entry.updated_at_secs)
                        >= self.time_threshold_secs.unwrap_or(u64::MAX)
            })
            .unwrap_or(false)
    }

    fn requeue_deferred_if_target_changed(&mut self, target_bytes: u64, now_secs: u64) {
        if self.target_bytes == Some(target_bytes) {
            return;
        }
        self.target_bytes = Some(target_bytes);
        let ids = self.deferred_oversize.iter().cloned().collect::<Vec<_>>();
        for id in ids {
            let fits = self
                .slices
                .get(&id)
                .map(|record| record.slice.index_entry.bytes <= target_bytes)
                .unwrap_or(false);
            if fits {
                self.classify_available(&id, now_secs);
            }
        }
    }

    fn remove_slice(&mut self, id: &CompactionSliceId) -> bool {
        let Some(record) = self.slices.get(id) else {
            return false;
        };
        let segment_key = record.slice.segment_key();
        self.remove_from_current_queue(id);
        self.waiting.remove(id);
        self.deferred_oversize.remove(id);
        self.force_only_ready.remove(id);
        self.slices.remove(id);

        let (remaining, groups, lanes, remove_segment) =
            if let Some(segment) = self.segments.get_mut(&segment_key) {
                segment.ids.remove(id);
                segment.remaining = segment.remaining.saturating_sub(1);
                (
                    segment.remaining,
                    segment.groups.clone(),
                    segment.lanes.clone(),
                    segment.remaining == 0,
                )
            } else {
                (0, BTreeSet::new(), BTreeSet::new(), false)
            };
        for group in groups {
            if let Some(queue) = self.groups.get_mut(&group) {
                queue.update_segment_remaining(&segment_key, remaining);
            }
        }
        for lane in lanes {
            if let Some(state) = self.lanes.get_mut(&lane) {
                state
                    .queue
                    .update_segment_remaining(&segment_key, remaining);
            }
        }
        if remove_segment {
            self.segments.remove(&segment_key);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::compaction_transaction::{SegmentSourceDescriptor, SinkWriteSemantics};
    use crate::buffer::segment_file::{PartitionKey, SegmentPartMetaSummary};
    use crate::plugins::source_contract::WritePolicy;
    use std::path::PathBuf;

    fn slice(
        segment: &str,
        sink_ref: &str,
        namespace: &str,
        schema: &str,
        kind: CompactionKind,
        start: u64,
        bytes: u64,
        updated_at_secs: u64,
    ) -> IndexedCompactionSlice {
        let key = PartitionKey {
            sink_ref: sink_ref.to_string(),
            namespace: namespace.to_string(),
            partition: "p=1".to_string(),
            time: Some(100),
            schema_fingerprint: schema.to_string(),
        };
        let source = SegmentSourceDescriptor::Disk {
            path: PathBuf::from(format!("/tmp/{segment}.seg")),
        };
        let index_entry = SegmentPartitionIndexEntry {
            key: key.clone(),
            bytes,
            updated_at_secs,
            slice_ordinal: start as u32,
            part_meta_start: 0,
            part_meta_len: 0,
            part_meta_summary: match kind {
                CompactionKind::Append => SegmentPartMetaSummary::Append,
                CompactionKind::Cdc => SegmentPartMetaSummary::Cdc {
                    row_count: 1,
                    canonical_hash: [start as u8; 32],
                },
            },
            start,
            len: bytes,
        };
        let wal_ref = WalPartRef {
            segment_id: segment.to_string(),
            source,
            start,
            len: bytes,
            key,
            cdc_meta_hash: index_entry.part_meta_summary.canonical_hash(),
        };
        IndexedCompactionSlice {
            id: CompactionSliceId::from_wal_ref(&wal_ref),
            group_key: CompactionGroupKey {
                sink_ref: sink_ref.to_string(),
                namespace: namespace.to_string(),
                partition: "p=1".to_string(),
                time: Some(100),
                schema_fingerprint: schema.to_string(),
                kind,
            },
            index_entry,
            wal_ref,
        }
    }

    fn plan(index: &mut CompactionIndex, limit: usize, force: bool) -> CompactionPlan {
        index.plan_ready(
            limit,
            force,
            1_000,
            100,
            100,
            250,
            2,
            limit.max(1),
            HashMap::new(),
            HashSet::new(),
        )
    }

    #[test]
    fn registration_builds_separate_typed_queues() {
        let mut index = CompactionIndex::default();
        index.register_segment(
            "seg",
            vec![
                slice(
                    "seg",
                    "sink.a",
                    "ns",
                    "v1",
                    CompactionKind::Append,
                    0,
                    100,
                    1,
                ),
                slice("seg", "sink.a", "ns", "v1", CompactionKind::Cdc, 1, 100, 1),
                slice(
                    "seg",
                    "sink.a",
                    "ns",
                    "v2",
                    CompactionKind::Append,
                    2,
                    100,
                    1,
                ),
            ],
            100,
            100,
            1_000,
        );
        assert_eq!(index.indexed_slice_count(), 3);
        assert_eq!(index.ready_queue_depth(), 3);
        assert_eq!(plan(&mut index, 3, false).groups.len(), 3);
    }

    #[test]
    fn reclaimable_slice_count_tracks_depth_within_one_ready_group() {
        let mut index = CompactionIndex::default();
        index.register_segment(
            "seg",
            vec![
                slice(
                    "seg",
                    "sink.a",
                    "ns",
                    "v1",
                    CompactionKind::Append,
                    0,
                    100,
                    1,
                ),
                slice(
                    "seg",
                    "sink.a",
                    "ns",
                    "v1",
                    CompactionKind::Append,
                    1,
                    100,
                    1,
                ),
                slice(
                    "seg",
                    "sink.a",
                    "ns",
                    "v1",
                    CompactionKind::Append,
                    2,
                    100,
                    1,
                ),
            ],
            100,
            100,
            1_000,
        );

        let planned = plan(&mut index, 1, false);
        assert_eq!(planned.groups.len(), 1);
        assert_eq!(planned.groups[0].slices.len(), 2);
        assert_eq!(index.ready_queue_depth(), 1);
        assert_eq!(
            index.reclaimable_slice_count(false, 1_000, 300, usize::MAX),
            1
        );
    }

    #[test]
    fn size_and_age_eligibility_are_incremental() {
        let mut index = CompactionIndex::default();
        index.register_segment(
            "seg",
            vec![
                slice(
                    "seg",
                    "sink.a",
                    "large",
                    "v1",
                    CompactionKind::Append,
                    0,
                    100,
                    999,
                ),
                slice(
                    "seg",
                    "sink.a",
                    "late",
                    "v1",
                    CompactionKind::Append,
                    1,
                    10,
                    800,
                ),
                slice(
                    "seg",
                    "sink.a",
                    "young",
                    "v1",
                    CompactionKind::Append,
                    2,
                    10,
                    999,
                ),
            ],
            100,
            100,
            1_000,
        );
        assert_eq!(index.ready_queue_depth(), 2);
        let planned = plan(&mut index, 3, false);
        assert_eq!(planned.groups.len(), 2);
        assert_eq!(planned.slices_examined, 2);
    }

    #[test]
    fn force_only_eligibility_does_not_leak_into_normal_mode() {
        let mut index = CompactionIndex::default();
        index.register_segment(
            "seg",
            vec![
                slice(
                    "seg",
                    "sink.a",
                    "ns",
                    "v1",
                    CompactionKind::Append,
                    0,
                    10,
                    999,
                ),
                slice(
                    "seg",
                    "sink.a",
                    "ns",
                    "v1",
                    CompactionKind::Append,
                    1,
                    10,
                    999,
                ),
            ],
            100,
            100,
            1_000,
        );
        let forced = index.plan_ready(
            1,
            true,
            1_000,
            100,
            100,
            250,
            1,
            1,
            HashMap::new(),
            HashSet::new(),
        );
        assert_eq!(forced.groups[0].slices.len(), 1);
        assert!(index
            .plan_ready(
                1,
                false,
                1_000,
                100,
                100,
                250,
                1,
                1,
                HashMap::new(),
                HashSet::new(),
            )
            .groups
            .is_empty());
    }

    #[test]
    fn hard_caps_hold_when_per_sink_limit_is_reached() {
        let mut index = CompactionIndex::default();
        let slices = (0..8)
            .map(|part| {
                slice(
                    "seg",
                    "sink.a",
                    "ns",
                    "v1",
                    CompactionKind::Append,
                    part,
                    100,
                    1,
                )
            })
            .collect();
        index.register_segment("seg", slices, 1, 100, 1_000);
        let planned = index.plan_ready(
            8,
            false,
            1_000,
            100,
            100,
            250,
            2,
            1,
            HashMap::new(),
            HashSet::new(),
        );
        assert_eq!(planned.groups.len(), 1);
        assert_eq!(planned.groups[0].slices.len(), 2);
        assert!(
            planned.groups[0]
                .slices
                .iter()
                .map(|slice| slice.index_entry.bytes)
                .sum::<u64>()
                <= 250
        );
        assert_eq!(index.ready_queue_depth(), 1);
    }

    #[test]
    fn per_sink_cap_includes_already_active_namespaces() {
        let mut index = CompactionIndex::default();
        index.register_segment(
            "seg-a",
            vec![slice(
                "seg-a",
                "sink.a",
                "ready-ns",
                "v1",
                CompactionKind::Append,
                0,
                100,
                1,
            )],
            1,
            100,
            1_000,
        );
        index.register_segment(
            "seg-b",
            vec![slice(
                "seg-b",
                "sink.b",
                "other",
                "v1",
                CompactionKind::Append,
                0,
                100,
                1,
            )],
            1,
            100,
            1_000,
        );
        let active = HashMap::from([(
            CompactionLaneKey {
                sink_ref: "sink.a".to_string(),
                namespace: "running-ns".to_string(),
            },
            1,
        )]);

        let planned = index.plan_ready(2, false, 1_000, 1, 100, 250, 1, 1, active, HashSet::new());

        assert_eq!(planned.groups.len(), 1);
        assert_eq!(planned.groups[0].key.sink_ref, "sink.b");
    }

    #[test]
    fn manifest_refs_are_fenced_until_manifest_removal() {
        let mut index = CompactionIndex::default();
        let indexed = slice(
            "seg",
            "sink.a",
            "ns",
            "v1",
            CompactionKind::Append,
            0,
            100,
            1,
        );
        let txn = CompactionTransaction::new(
            "sink.a".to_string(),
            "ns".to_string(),
            "v1".to_string(),
            WritePolicy::Append,
            SinkWriteSemantics::ExactOnce,
            vec![indexed.wal_ref.clone()],
            "out".to_string(),
        );
        index.install_manifests(vec![txn.clone()]);
        index.register_segment("seg", vec![indexed], 1, 100, 1_000);
        assert!(plan(&mut index, 1, false).groups.is_empty());
        assert_eq!(
            index
                .reserve_ready_manifests(1, 1, 1_000, 300, &HashMap::new(), &HashSet::new())
                .len(),
            1
        );
        index.remove_manifest(&txn.id, 1_000);
        assert_eq!(plan(&mut index, 1, false).groups.len(), 1);
    }

    #[test]
    fn sent_manifest_remains_fenced_until_stale() {
        let mut index = CompactionIndex::default();
        let indexed = slice(
            "seg",
            "sink.a",
            "ns",
            "v1",
            CompactionKind::Append,
            0,
            100,
            1,
        );
        let mut txn = CompactionTransaction::new(
            "sink.a".to_string(),
            "ns".to_string(),
            "v1".to_string(),
            WritePolicy::Append,
            SinkWriteSemantics::ExactOnce,
            vec![indexed.wal_ref.clone()],
            "out".to_string(),
        )
        .mark_sent();
        txn.updated_at_secs = 900;
        index.install_manifests(vec![txn]);
        index.register_segment("seg", vec![indexed], 1, 100, 1_000);

        assert!(index
            .reserve_ready_manifests(1, 1, 1_100, 300, &HashMap::new(), &HashSet::new())
            .is_empty());
        assert!(plan(&mut index, 1, true).groups.is_empty());
        let retry =
            index.reserve_ready_manifests(1, 1, 1_200, 300, &HashMap::new(), &HashSet::new());
        assert_eq!(retry.len(), 1);
        assert!(retry[0].retried_stale);
        assert_eq!(retry[0].stale_age_secs, 300);
        assert_eq!(retry[0].txn.state, CompactionTransactionState::Pending);
    }

    #[test]
    fn fairness_serves_each_sink_namespace_before_repeating() {
        let mut index = CompactionIndex::default();
        for (ordinal, (sink, namespace)) in [("sink.a", "a"), ("sink.a", "b"), ("sink.b", "a")]
            .into_iter()
            .enumerate()
        {
            index.register_segment(
                &format!("seg-{ordinal}"),
                vec![slice(
                    &format!("seg-{ordinal}"),
                    sink,
                    namespace,
                    "v1",
                    CompactionKind::Append,
                    0,
                    100,
                    10 + ordinal as u64,
                )],
                1,
                100,
                1_000,
            );
        }
        let mut served = Vec::new();
        for _ in 0..3 {
            let planned = index.plan_ready(
                1,
                false,
                1_000,
                1,
                100,
                250,
                2,
                2,
                HashMap::new(),
                HashSet::new(),
            );
            let group = &planned.groups[0];
            served.push((group.key.sink_ref.clone(), group.key.namespace.clone()));
        }
        assert_eq!(
            served.into_iter().collect::<BTreeSet<_>>().len(),
            3,
            "every sink/namespace lane must receive a turn"
        );
    }

    #[test]
    fn completion_removes_slices_and_prioritizes_closing_segments() {
        let mut index = CompactionIndex::default();
        let closing = slice(
            "closing",
            "sink.a",
            "ns",
            "v1",
            CompactionKind::Append,
            0,
            100,
            1,
        );
        let busy_a = slice(
            "busy",
            "sink.a",
            "ns",
            "v1",
            CompactionKind::Append,
            0,
            100,
            1,
        );
        let busy_b = slice(
            "busy",
            "sink.a",
            "other",
            "v1",
            CompactionKind::Append,
            1,
            100,
            1,
        );
        index.register_segment("closing", vec![closing.clone()], 1, 100, 1_000);
        index.register_segment("busy", vec![busy_a, busy_b], 1, 100, 1_000);
        let planned = plan(&mut index, 1, false);
        assert_eq!(planned.groups[0].slices[0].id.segment_id, "closing");
        assert_eq!(index.complete_refs(&[closing.wal_ref]), 1);
        assert_eq!(index.indexed_slice_count(), 2);
    }

    #[test]
    fn recovery_rebuild_registers_only_current_slices() {
        let mut index = CompactionIndex::default();
        index.register_segment(
            "old",
            vec![slice(
                "old",
                "sink.a",
                "ns",
                "v1",
                CompactionKind::Append,
                0,
                100,
                1,
            )],
            1,
            100,
            1_000,
        );
        index.clear_for_recovery();
        index.register_segment(
            "new",
            vec![slice(
                "new",
                "sink.a",
                "ns",
                "v1",
                CompactionKind::Append,
                0,
                100,
                1,
            )],
            1,
            100,
            1_000,
        );
        assert_eq!(index.indexed_slice_count(), 1);
        assert_eq!(
            plan(&mut index, 1, false).groups[0].slices[0].id.segment_id,
            "new"
        );
    }

    #[test]
    fn synthetic_100k_slices_plans_without_full_scan() {
        let mut index = CompactionIndex::default();
        for segment in 0..1_000u64 {
            let slices = (0..100u64)
                .map(|part| {
                    slice(
                        &format!("seg-{segment}"),
                        "sink.a",
                        "ns",
                        "v1",
                        CompactionKind::Append,
                        part,
                        1,
                        1,
                    )
                })
                .collect();
            index.register_segment(&format!("seg-{segment}"), slices, 1, 100, 1_000);
        }
        let planned = index.plan_ready(
            1,
            false,
            1_000,
            1,
            100,
            1_024,
            128,
            1,
            HashMap::new(),
            HashSet::new(),
        );
        assert_eq!(planned.groups.len(), 1);
        assert_eq!(planned.groups[0].slices.len(), 128);
        assert!(planned.slices_examined <= 129);
        assert!(planned.segments_examined <= 2);
    }

    #[test]
    fn drop_segments_not_in_live_set_removes_slices() {
        let mut index = CompactionIndex::default();
        index.register_segment(
            "keep",
            vec![slice(
                "keep",
                "sink.a",
                "ns",
                "v1",
                CompactionKind::Append,
                0,
                100,
                1,
            )],
            1_000,
            100,
            1,
        );
        index.register_segment(
            "gone",
            vec![slice(
                "gone",
                "sink.a",
                "ns",
                "v1",
                CompactionKind::Append,
                0,
                100,
                1,
            )],
            1_000,
            100,
            1,
        );
        assert_eq!(index.indexed_slice_count(), 2);
        index.drop_segments_not_in(&HashSet::from(["keep".to_string()]));
        assert_eq!(index.indexed_slice_count(), 1);
    }

    #[test]
    fn complete_ordinals_removes_matching_slices() {
        let mut index = CompactionIndex::default();
        index.register_segment(
            "seg",
            vec![
                slice(
                    "seg",
                    "sink.a",
                    "ns",
                    "v1",
                    CompactionKind::Append,
                    0,
                    100,
                    1,
                ),
                slice(
                    "seg",
                    "sink.a",
                    "ns",
                    "v1",
                    CompactionKind::Append,
                    1,
                    100,
                    1,
                ),
            ],
            1_000,
            100,
            1,
        );
        assert_eq!(index.complete_ordinals("seg", &[0]), 1);
        assert_eq!(index.indexed_slice_count(), 1);
    }
}
