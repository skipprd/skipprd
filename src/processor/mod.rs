//! Skippr Store `stream_state` family: keyed aggregates over committed WAL.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use arrow::array::{Array, RecordBatch};
use arrow::datatypes::{DataType, TimeUnit};
use serde_derive::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, error, info, warn};

use crate::buffer::segment_file::SegmentFile;
use crate::compute::{name_for_field_id, ComputeKind, Measure, MeasureKind};
use crate::helpers::configuration::Config;
use crate::plugins::cdc::{project_pk_change, CdcEvent, MutationKind, Tie, WalRowMeta};
use skippr_wal_catalog::{
    CatalogFence, KvOp, SegmentRow, SkipprStore, SkipprStoreRead, StoreFamily,
};

/// Next unconsumed catalog seq per pipeline. Missing key means no processor is holding reclaim (`u64::MAX`).
#[cfg(not(test))]
static MIN_UNCONSUMED_SEQ: LazyLock<Mutex<HashMap<String, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(test)]
thread_local! {
    static MIN_UNCONSUMED_SEQ: std::cell::RefCell<HashMap<String, u64>> =
        std::cell::RefCell::new(HashMap::new());
}

#[cfg(not(test))]
static COMPUTE_CURSORS: LazyLock<Mutex<HashMap<String, HashMap<String, u64>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(test)]
thread_local! {
    static COMPUTE_CURSORS: std::cell::RefCell<HashMap<String, HashMap<String, u64>>> =
        std::cell::RefCell::new(HashMap::new());
}

/// One in-memory snapshot per compaction run. Readers never see a half update.
#[cfg(not(test))]
static COMPACTION_SNAPSHOT: LazyLock<Mutex<HashMap<String, Arc<Option<u64>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(test)]
thread_local! {
    static COMPACTION_SNAPSHOT: std::cell::RefCell<HashMap<String, Arc<Option<u64>>>> =
        std::cell::RefCell::new(HashMap::new());
}
static PROCESSOR_STARTED: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PROCESSOR_STOP: AtomicBool = AtomicBool::new(false);
static PROCESSOR_HANDLE: LazyLock<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    BeforeTxn,
    BeforeCommit,
    DuringTxn,
    AfterTxn,
}

#[cfg(test)]
thread_local! {
    static TEST_CRASH: std::cell::Cell<Option<CrashPoint>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub fn set_test_crash(point: Option<CrashPoint>) {
    TEST_CRASH.with(|cell| cell.set(point));
}

fn maybe_crash(point: CrashPoint) {
    #[cfg(test)]
    {
        TEST_CRASH.with(|cell| {
            if cell.get() == Some(point) {
                panic!("stream processor crash injection {point:?}");
            }
        });
    }
    let _ = point;
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WalCursor {
    pub seq: u64,
    pub segment_id: String,
    pub slice_ordinal: u32,
    pub row_index: u64,
}

impl WalCursor {
    pub fn is_before(&self, seq: u64, segment_id: &str, slice: u32, row: u64) -> bool {
        (
            self.seq,
            self.segment_id.as_str(),
            self.slice_ordinal,
            self.row_index,
        ) < (seq, segment_id, slice, row)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct ProcessorMeta {
    pub(crate) cursor: WalCursor,
}

pub fn min_unconsumed_seq(pipeline: &str) -> u64 {
    #[cfg(test)]
    {
        return MIN_UNCONSUMED_SEQ
            .with(|slot| slot.borrow().get(pipeline).copied().unwrap_or(u64::MAX));
    }
    #[cfg(not(test))]
    {
        MIN_UNCONSUMED_SEQ
            .lock()
            .ok()
            .and_then(|map| map.get(pipeline).copied())
            .unwrap_or(u64::MAX)
    }
}

pub fn allows_reclaim(pipeline: &str, segment_seq: u64) -> bool {
    skippr_cdc::compaction_may_remove(compaction_minimum(pipeline), segment_seq)
}

pub fn compaction_may_size(segment_seq: u64) -> bool {
    skippr_cdc::compaction_may_remove(compaction_snapshot_min(), segment_seq)
}

fn compaction_minimum(pipeline: &str) -> Option<u64> {
    let seq = min_unconsumed_seq(pipeline);
    if seq == u64::MAX {
        None
    } else {
        Some(seq)
    }
}

/// Load the global minimum once for this compaction run. The swap is atomic.
pub fn load_compaction_snapshot(pipeline: &str) -> Arc<Option<u64>> {
    let snap = Arc::new(compaction_minimum(pipeline));
    #[cfg(test)]
    {
        COMPACTION_SNAPSHOT.with(|slot| {
            slot.borrow_mut()
                .insert(pipeline.to_string(), Arc::clone(&snap));
        });
        return snap;
    }
    #[cfg(not(test))]
    {
        if let Ok(mut map) = COMPACTION_SNAPSHOT.lock() {
            map.insert(pipeline.to_string(), Arc::clone(&snap));
        }
        snap
    }
}

fn compaction_snapshot_min() -> Option<u64> {
    #[cfg(test)]
    {
        return COMPACTION_SNAPSHOT
            .with(|slot| slot.borrow().values().next().and_then(|snap| **snap));
    }
    #[cfg(not(test))]
    {
        COMPACTION_SNAPSHOT
            .lock()
            .ok()
            .and_then(|map| map.values().next().cloned())
            .and_then(|snap| *snap)
    }
}

pub fn set_min_unconsumed_seq(pipeline: &str, seq: u64) {
    #[cfg(test)]
    {
        MIN_UNCONSUMED_SEQ.with(|slot| {
            slot.borrow_mut().insert(pipeline.to_string(), seq);
        });
    }
    #[cfg(not(test))]
    {
        if let Ok(mut map) = MIN_UNCONSUMED_SEQ.lock() {
            map.insert(pipeline.to_string(), seq);
        }
    }
}

/// Per-compute cursor. The stored global minimum is the lowest of these.
/// A faster compute cannot raise it past a slower one: a stale higher read
/// loses the compare-and-swap.
pub fn set_compute_cursor(pipeline: &str, compute_id: &str, seq: u64) {
    let cursors = with_cursors(pipeline, |map| {
        map.insert(compute_id.to_string(), seq);
        map.values().copied().collect::<Vec<_>>()
    });
    let proposed = skippr_cdc::global_min(&cursors);
    cas_global_min(pipeline, proposed);
}

fn with_cursors<R>(pipeline: &str, f: impl FnOnce(&mut HashMap<String, u64>) -> R) -> R {
    #[cfg(test)]
    {
        return COMPUTE_CURSORS.with(|slot| {
            let mut map = slot.borrow_mut();
            f(map.entry(pipeline.to_string()).or_default())
        });
    }
    #[cfg(not(test))]
    {
        let mut map = COMPUTE_CURSORS.lock().expect("compute cursors");
        f(map.entry(pipeline.to_string()).or_default())
    }
}

fn cas_global_min(pipeline: &str, proposed: Option<u64>) {
    let Some(proposed) = proposed else {
        set_min_unconsumed_seq(pipeline, u64::MAX);
        return;
    };
    loop {
        let current = min_unconsumed_seq(pipeline);
        if current == u64::MAX || proposed <= current {
            set_min_unconsumed_seq(pipeline, proposed);
            return;
        }
        let fresh = with_cursors(pipeline, |map| {
            skippr_cdc::global_min(&map.values().copied().collect::<Vec<_>>())
        });
        match fresh {
            Some(min) if min == proposed => {
                set_min_unconsumed_seq(pipeline, proposed);
                return;
            }
            Some(min) if min < proposed => return,
            _ => return,
        }
    }
}

pub fn reset_reclaim_hold() {
    #[cfg(test)]
    {
        MIN_UNCONSUMED_SEQ.with(|slot| slot.borrow_mut().clear());
        COMPUTE_CURSORS.with(|slot| slot.borrow_mut().clear());
        COMPACTION_SNAPSHOT.with(|slot| slot.borrow_mut().clear());
    }
    #[cfg(not(test))]
    {
        if let Ok(mut map) = MIN_UNCONSUMED_SEQ.lock() {
            map.clear();
        }
        if let Ok(mut map) = COMPUTE_CURSORS.lock() {
            map.clear();
        }
        if let Ok(mut map) = COMPACTION_SNAPSHOT.lock() {
            map.clear();
        }
    }
}

pub struct StreamState {
    store: Arc<dyn SkipprStore>,
    fence: CatalogFence,
    pipeline: String,
    kinds: Vec<ComputeKind>,
}

impl StreamState {
    pub fn open(
        store: Arc<dyn SkipprStore>,
        fence: CatalogFence,
        pipeline: impl Into<String>,
        kinds: Vec<ComputeKind>,
    ) -> Result<Self, String> {
        let pipeline = pipeline.into();
        let state = Self {
            store,
            fence,
            pipeline,
            kinds,
        };
        state.ensure_cursors()?;
        Ok(state)
    }

    fn family_key(&self, rest: &[u8]) -> Vec<u8> {
        StoreFamily::StreamState.key(rest)
    }

    fn get_raw(&self, rest: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.store
            .get(&self.family_key(rest))
            .map_err(|err| err.to_string())
    }

    fn commit_ops(&self, ops: Vec<KvOp>) -> Result<(), String> {
        if ops.is_empty() {
            return Ok(());
        }
        self.store
            .commit(&ops, self.fence)
            .map_err(|err| err.to_string())?;
        self.store.flush_durable().map_err(|err| err.to_string())
    }

    fn meta_key(&self, id: &str) -> Vec<u8> {
        format!("meta/{}/{id}", self.pipeline).into_bytes()
    }

    fn agg_key(&self, id: &str, key: &str) -> Vec<u8> {
        format!("agg/{}/{id}/{key}", self.pipeline).into_bytes()
    }

    fn row_key(&self, id: &str, key: &str) -> Vec<u8> {
        format!("row/{}/{id}/{key}", self.pipeline).into_bytes()
    }

    fn dedup_key(&self, id: &str, event_id: &[u8]) -> Vec<u8> {
        format!("dedup/{}/{id}/{}", self.pipeline, hex::encode(event_id)).into_bytes()
    }

    fn prefix(&self, kind: &str, id: &str) -> Vec<u8> {
        format!("{kind}/{}/{id}/", self.pipeline).into_bytes()
    }

    fn ensure_cursors(&self) -> Result<(), String> {
        for kind in self.kinds.iter().filter(|k| k.is_store_table()) {
            let Some(id) = kind.store_id() else {
                continue;
            };
            if self.load_meta(id)?.is_none() {
                self.write_meta(
                    id,
                    &ProcessorMeta {
                        cursor: WalCursor::default(),
                    },
                )?;
            }
        }
        if let Some(min_seq) = self
            .kinds
            .iter()
            .filter_map(|kind| {
                kind.store_id().and_then(|id| {
                    self.load_meta(id)
                        .ok()
                        .flatten()
                        .map(|meta| meta.cursor.seq)
                })
            })
            .min()
        {
            set_min_unconsumed_seq(&self.pipeline, min_seq);
        }
        Ok(())
    }

    fn write_meta(&self, id: &str, meta: &ProcessorMeta) -> Result<(), String> {
        let bytes = serde_json::to_vec(meta).map_err(|err| err.to_string())?;
        self.commit_ops(vec![KvOp::Put {
            key: self.family_key(&self.meta_key(id)),
            value: bytes,
        }])
    }

    pub fn pipeline(&self) -> &str {
        &self.pipeline
    }

    pub(crate) fn load_meta(&self, id: &str) -> Result<Option<ProcessorMeta>, String> {
        let bytes = self.get_raw(&self.meta_key(id))?;
        bytes
            .map(|b| serde_json::from_slice(&b).map_err(|err| err.to_string()))
            .transpose()
    }

    pub fn measure(&self, id: &str, key: &str, measure: &str) -> Result<f64, String> {
        let bytes = self.get_raw(&self.agg_key(id, key))?;
        let Some(bytes) = bytes else {
            return Ok(0.0);
        };
        let map: BTreeMap<String, f64> =
            serde_json::from_slice(&bytes).map_err(|err| err.to_string())?;
        Ok(*map.get(measure).unwrap_or(&0.0))
    }

    pub fn apply_events(
        &self,
        agg: &ComputeKind,
        cursor: WalCursor,
        batch: &RecordBatch,
        cdc_rows: Option<&[WalRowMeta]>,
        start_row: usize,
    ) -> Result<WalCursor, String> {
        if start_row >= batch.num_rows() {
            return Ok(cursor);
        }
        let started = std::time::Instant::now();
        maybe_crash(CrashPoint::BeforeTxn);
        let mut measures: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
        let mut last_rows: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
        let mut dedup: Vec<(Vec<u8>, AppliedCopy)> = Vec::new();
        let mut next_cursor = cursor.clone();

        for row in start_row..batch.num_rows() {
            let meta = cdc_rows.and_then(|rows| rows.get(row));
            let new_row = row_values(batch, row)?;
            let key = encode_key_ids(agg, batch.schema().as_ref(), &new_row)?;
            let mutation = meta.map(|m| m.mutation).unwrap_or(MutationKind::Insert);
            let mut applied: Vec<(Vec<u8>, MutationKind, String, BTreeMap<String, Value>)> =
                Vec::new();
            if let Some(meta) = meta {
                if mutation == MutationKind::Update && meta.before.is_none() {
                    next_cursor.row_index = (row as u64) + 1;
                    continue;
                }
                let event = CdcEvent::from_wal(
                    meta,
                    Tie {
                        seq: next_cursor.seq,
                        ordinal: next_cursor.slice_ordinal,
                        row: row as u64,
                    },
                );
                let halves = project_pk_change(event, key.as_bytes());
                let candidate = crate::plugins::cdc::StoredWinner {
                    order: meta.order_token.clone(),
                    tie: halves[0].tie.unwrap_or(Tie {
                        seq: next_cursor.seq,
                        ordinal: next_cursor.slice_ordinal,
                        row: row as u64,
                    }),
                };
                let mut skip = false;
                let mut retracts = Vec::new();
                for half in &halves {
                    if half.event_id.is_empty() {
                        continue;
                    }
                    let dkey = self.dedup_key(store_id(agg)?, &half.event_id);
                    let stored = load_applied_copy(self.get_raw(&dkey)?)?;
                    let pending = dedup
                        .iter()
                        .rev()
                        .find(|(key, _)| key == &dkey)
                        .map(|(_, copy)| copy.clone());
                    let incumbent = pending.as_ref().or(stored.as_ref());
                    if let Some(incumbent) = incumbent {
                        if !crate::plugins::cdc::beats(&candidate, &incumbent.winner()) {
                            skip = true;
                            break;
                        }
                        retracts.push(incumbent.clone());
                    }
                }
                if skip {
                    next_cursor.row_index = (row as u64) + 1;
                    continue;
                }
                for previous in &retracts {
                    retract_applied(
                        agg,
                        batch.schema().as_ref(),
                        previous,
                        &mut measures,
                        &mut last_rows,
                    )?;
                }
                if halves.len() == 2 {
                    let Some(old) = parse_before_row(meta) else {
                        next_cursor.row_index = (row as u64) + 1;
                        continue;
                    };
                    let old_key = encode_key_ids(agg, batch.schema().as_ref(), &old)?;
                    apply_mutation(
                        agg,
                        batch.schema().as_ref(),
                        MutationKind::Delete,
                        &old_key,
                        Some(&old),
                        &new_row,
                        &mut measures,
                        &mut last_rows,
                    )?;
                    apply_mutation(
                        agg,
                        batch.schema().as_ref(),
                        MutationKind::Insert,
                        &key,
                        None,
                        &new_row,
                        &mut measures,
                        &mut last_rows,
                    )?;
                    applied.push((
                        halves[0].event_id.clone(),
                        MutationKind::Delete,
                        old_key,
                        old,
                    ));
                    applied.push((
                        halves[1].event_id.clone(),
                        MutationKind::Insert,
                        key.clone(),
                        new_row.clone(),
                    ));
                } else {
                    let old_row = match mutation {
                        MutationKind::Insert | MutationKind::Snapshot => None,
                        MutationKind::Update => parse_before_row(meta),
                        MutationKind::Delete => match parse_before_row(meta) {
                            Some(old) => Some(old),
                            None => self.same_key_row(store_id(agg)?, &key, &last_rows)?,
                        },
                    };
                    apply_mutation(
                        agg,
                        batch.schema().as_ref(),
                        mutation,
                        &key,
                        old_row.as_ref(),
                        &new_row,
                        &mut measures,
                        &mut last_rows,
                    )?;
                    applied.push((
                        meta.event_id.clone(),
                        mutation,
                        key.clone(),
                        new_row.clone(),
                    ));
                }
            } else {
                apply_mutation(
                    agg,
                    batch.schema().as_ref(),
                    MutationKind::Insert,
                    &key,
                    None,
                    &new_row,
                    &mut measures,
                    &mut last_rows,
                )?;
            }
            for (event_id, applied_mutation, applied_key, values) in applied {
                if event_id.is_empty() {
                    continue;
                }
                let dkey = self.dedup_key(store_id(agg)?, &event_id);
                dedup.retain(|(key, _)| key != &dkey);
                dedup.push((
                    dkey,
                    AppliedCopy {
                        order: meta.map(|m| m.order_token.clone()).unwrap_or_default(),
                        seq: next_cursor.seq,
                        ordinal: next_cursor.slice_ordinal,
                        row: row as u64,
                        key: applied_key,
                        mutation: applied_mutation,
                        values,
                    },
                ));
            }
            match mutation {
                MutationKind::Delete => {
                    crate::metrics::counters::add_stream_processor_cdc_delete(1);
                }
                MutationKind::Update => {
                    crate::metrics::counters::add_stream_processor_cdc_update(1);
                }
                MutationKind::Insert | MutationKind::Snapshot => {
                    crate::metrics::counters::add_stream_processor_cdc_insert(1);
                }
            }
            next_cursor.row_index = (row as u64) + 1;
        }
        crate::metrics::counters::add_stream_processor_records(
            (batch.num_rows() - start_row) as u64,
        );
        crate::helpers::perf::emit(
            "stream_processor",
            serde_json::json!({
                "elapsed_ms": started.elapsed().as_millis() as u64,
                "rows": batch.num_rows() - start_row,
                "aggregate": store_id(agg).unwrap_or("")
            }),
        );

        let meta = ProcessorMeta {
            cursor: next_cursor.clone(),
        };
        let meta_bytes = serde_json::to_vec(&meta).map_err(|err| err.to_string())?;
        let meta_key = self.meta_key(store_id(agg)?);
        let mut puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        puts.push((meta_key, meta_bytes));
        for (key, values) in &measures {
            let mut current = self.load_measures(store_id(agg)?, key)?;
            for (name, delta) in values {
                *current.entry(name.clone()).or_insert(0.0) += *delta;
            }
            puts.push((
                self.agg_key(store_id(agg)?, key),
                serde_json::to_vec(&current).map_err(|err| err.to_string())?,
            ));
        }
        for (key, row) in &last_rows {
            puts.push((
                self.row_key(store_id(agg)?, key),
                serde_json::to_vec(row).map_err(|err| err.to_string())?,
            ));
        }
        for (dkey, copy) in &dedup {
            puts.push((
                dkey.clone(),
                serde_json::to_vec(copy).map_err(|err| err.to_string())?,
            ));
        }
        puts.push((
            format!("min/{}/{}", self.pipeline, store_id(agg).unwrap_or("")).into_bytes(),
            next_cursor.seq.to_le_bytes().to_vec(),
        ));

        maybe_crash(CrashPoint::BeforeCommit);
        maybe_crash(CrashPoint::DuringTxn);
        let ops: Vec<KvOp> = puts
            .into_iter()
            .map(|(key, value)| KvOp::Put {
                key: self.family_key(&key),
                value,
            })
            .collect();
        if let Err(err) = self.commit_ops(ops) {
            crate::metrics::counters::add_stream_processor_txn_failure(1);
            return Err(format!("stream processor transaction failed: {err}"));
        }
        maybe_crash(CrashPoint::AfterTxn);
        set_compute_cursor(&self.pipeline, store_id(agg).unwrap_or(""), next_cursor.seq);
        Ok(next_cursor)
    }

    fn same_key_row(
        &self,
        id: &str,
        key: &str,
        pending: &BTreeMap<String, BTreeMap<String, Value>>,
    ) -> Result<Option<BTreeMap<String, Value>>, String> {
        if let Some(row) = pending.get(key).cloned().filter(|row| !row.is_empty()) {
            return Ok(Some(row));
        }
        self.load_last_row(id, key)
    }

    fn load_last_row(
        &self,
        id: &str,
        key: &str,
    ) -> Result<Option<BTreeMap<String, Value>>, String> {
        let bytes = self.get_raw(&self.row_key(id, key))?;
        match bytes {
            Some(bytes) => {
                let map: BTreeMap<String, Value> =
                    serde_json::from_slice(&bytes).map_err(|err| err.to_string())?;
                if map.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(map))
                }
            }
            None => Ok(None),
        }
    }

    fn load_measures(&self, id: &str, key: &str) -> Result<BTreeMap<String, f64>, String> {
        let bytes = self.get_raw(&self.agg_key(id, key))?;
        match bytes {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|err| err.to_string()),
            None => Ok(BTreeMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn snapshot_kv(&self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        self.store
            .query_prefix(StoreFamily::StreamState.prefix(), None, usize::MAX)
            .unwrap_or_default()
            .into_iter()
            .collect()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AppliedCopy {
    order: Vec<u8>,
    seq: u64,
    ordinal: u32,
    row: u64,
    key: String,
    mutation: MutationKind,
    values: BTreeMap<String, Value>,
}

impl AppliedCopy {
    fn winner(&self) -> crate::plugins::cdc::StoredWinner {
        crate::plugins::cdc::StoredWinner {
            order: self.order.clone(),
            tie: crate::plugins::cdc::Tie {
                seq: self.seq,
                ordinal: self.ordinal,
                row: self.row,
            },
        }
    }
}

fn load_applied_copy(bytes: Option<Vec<u8>>) -> Result<Option<AppliedCopy>, String> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    if bytes == b"1" {
        return Ok(None);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| err.to_string())
}

fn retract_applied(
    agg: &ComputeKind,
    schema: &arrow_schema::Schema,
    previous: &AppliedCopy,
    measures: &mut BTreeMap<String, BTreeMap<String, f64>>,
    last_rows: &mut BTreeMap<String, BTreeMap<String, Value>>,
) -> Result<(), String> {
    let inverse = match previous.mutation {
        MutationKind::Delete => MutationKind::Insert,
        MutationKind::Insert | MutationKind::Snapshot | MutationKind::Update => {
            MutationKind::Delete
        }
    };
    apply_mutation(
        agg,
        schema,
        inverse,
        &previous.key,
        Some(&previous.values),
        &previous.values,
        measures,
        last_rows,
    )
}

fn parse_before_row(meta: &WalRowMeta) -> Option<BTreeMap<String, Value>> {
    meta.before
        .as_ref()
        .and_then(|before| serde_json::from_slice(&before.row).ok())
}

fn apply_mutation(
    agg: &ComputeKind,
    schema: &arrow_schema::Schema,
    mutation: MutationKind,
    new_key: &str,
    old_row: Option<&BTreeMap<String, Value>>,
    new_row: &BTreeMap<String, Value>,
    measures: &mut BTreeMap<String, BTreeMap<String, f64>>,
    last_rows: &mut BTreeMap<String, BTreeMap<String, Value>>,
) -> Result<(), String> {
    let old_key = old_row
        .map(|row| encode_key_ids(agg, schema, row))
        .transpose()?;
    let pk_changed = old_key.as_deref().is_some_and(|old| old != new_key);

    match mutation {
        MutationKind::Delete => {
            if let Some(old) = old_row {
                add_deltas(
                    agg,
                    schema,
                    old_key.as_deref().unwrap_or(new_key),
                    old,
                    -1.0,
                    measures,
                )?;
            }
            last_rows.insert(new_key.to_string(), BTreeMap::new());
            if let Some(old) = &old_key {
                if old != new_key {
                    last_rows.insert(old.clone(), BTreeMap::new());
                }
            }
        }
        MutationKind::Insert | MutationKind::Snapshot => {
            add_deltas(agg, schema, new_key, new_row, 1.0, measures)?;
            last_rows.insert(new_key.to_string(), new_row.clone());
        }
        MutationKind::Update => {
            if pk_changed {
                if let Some(old) = old_row {
                    add_deltas(agg, schema, old_key.as_ref().unwrap(), old, -1.0, measures)?;
                    last_rows.insert(old_key.unwrap(), BTreeMap::new());
                }
                add_deltas(agg, schema, new_key, new_row, 1.0, measures)?;
            } else if let Some(old) = old_row {
                add_measure_delta(agg, schema, new_key, old, new_row, measures)?;
            } else {
                add_deltas(agg, schema, new_key, new_row, 1.0, measures)?;
            }
            last_rows.insert(new_key.to_string(), new_row.clone());
        }
    }
    Ok(())
}

fn add_deltas(
    agg: &ComputeKind,
    schema: &arrow_schema::Schema,
    key: &str,
    row: &BTreeMap<String, Value>,
    sign: f64,
    measures: &mut BTreeMap<String, BTreeMap<String, f64>>,
) -> Result<(), String> {
    let slot = measures.entry(key.to_string()).or_default();
    for measure in agg.measures() {
        *slot.entry(measure.name.as_str().to_string()).or_insert(0.0) +=
            sign * measure_value(measure, schema, row)?;
    }
    Ok(())
}

fn add_measure_delta(
    agg: &ComputeKind,
    schema: &arrow_schema::Schema,
    key: &str,
    old_row: &BTreeMap<String, Value>,
    new_row: &BTreeMap<String, Value>,
    measures: &mut BTreeMap<String, BTreeMap<String, f64>>,
) -> Result<(), String> {
    let slot = measures.entry(key.to_string()).or_default();
    for measure in agg.measures() {
        let delta =
            measure_value(measure, schema, new_row)? - measure_value(measure, schema, old_row)?;
        *slot.entry(measure.name.as_str().to_string()).or_insert(0.0) += delta;
    }
    Ok(())
}

fn measure_value(
    measure: &Measure,
    schema: &arrow_schema::Schema,
    row: &BTreeMap<String, Value>,
) -> Result<f64, String> {
    match measure.kind {
        MeasureKind::Count => Ok(1.0),
        MeasureKind::Sum(id) | MeasureKind::Min(id) | MeasureKind::Max(id) => {
            let name = name_for_field_id(schema, id)
                .ok_or_else(|| format!("FieldId {} missing from batch", id.get()))?;
            Ok(json_f64(row.get(name).unwrap_or(&Value::Null)))
        }
    }
}

fn json_f64(value: &Value) -> f64 {
    match value {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Bool(true) => 1.0,
        _ => 0.0,
    }
}

fn store_id(kind: &ComputeKind) -> Result<&str, String> {
    kind.store_id()
        .ok_or_else(|| "compute has no Store name".into())
}

fn encode_key_ids(
    kind: &ComputeKind,
    schema: &arrow_schema::Schema,
    row: &BTreeMap<String, Value>,
) -> Result<String, String> {
    let ids = kind
        .keys()
        .ok_or_else(|| "compute has no keys".to_string())?;
    let mut parts = Vec::with_capacity(ids.len() + 1);
    for id in ids {
        let name = name_for_field_id(schema, *id)
            .ok_or_else(|| format!("FieldId {} missing from batch", id.get()))?;
        parts.push(json_key_part(row.get(name).unwrap_or(&Value::Null)));
    }
    if let ComputeKind::WindowedTable { window, .. } = kind {
        parts.push(tumble_bucket(window, schema, row)?);
    }
    Ok(parts.join("\u{1f}"))
}

fn tumble_bucket(
    window: &crate::compute::Tumble,
    schema: &arrow_schema::Schema,
    row: &BTreeMap<String, Value>,
) -> Result<String, String> {
    let name = name_for_field_id(schema, window.on.field_id()).ok_or_else(|| {
        format!(
            "EventTimeCol FieldId {} missing from batch",
            window.on.field_id().get()
        )
    })?;
    let ts = event_time_unix(row.get(name).unwrap_or(&Value::Null))?;
    let size = window.size.as_secs();
    if size == 0 {
        return Err("Tumble size must be non-zero".into());
    }
    Ok((ts.div_euclid(size as i64) * size as i64).to_string())
}

fn event_time_unix(value: &Value) -> Result<i64, String> {
    match value {
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.timestamp())
            .map_err(|err| err.to_string()),
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| "event_time number is not i64".into()),
        Value::Null => Err("event_time is null".into()),
        other => Err(format!("event_time value {other}")),
    }
}

fn json_key_part(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn row_values(batch: &RecordBatch, row: usize) -> Result<BTreeMap<String, Value>, String> {
    let mut out = BTreeMap::new();
    for (i, field) in batch.schema().fields().iter().enumerate() {
        out.insert(
            field.name().clone(),
            array_value(batch.column(i).as_ref(), row)?,
        );
    }
    Ok(out)
}

fn array_value(array: &dyn Array, row: usize) -> Result<Value, String> {
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    match array.data_type() {
        DataType::Utf8 => {
            let arr = array
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .ok_or("utf8")?;
            Ok(Value::String(arr.value(row).to_string()))
        }
        DataType::LargeUtf8 => {
            let arr = array
                .as_any()
                .downcast_ref::<arrow::array::LargeStringArray>()
                .ok_or("largeutf8")?;
            Ok(Value::String(arr.value(row).to_string()))
        }
        DataType::Int8 => Ok(Value::from(
            array
                .as_any()
                .downcast_ref::<arrow::array::Int8Array>()
                .ok_or("int8")?
                .value(row),
        )),
        DataType::Int16 => Ok(Value::from(
            array
                .as_any()
                .downcast_ref::<arrow::array::Int16Array>()
                .ok_or("int16")?
                .value(row),
        )),
        DataType::Int32 => Ok(Value::from(
            array
                .as_any()
                .downcast_ref::<arrow::array::Int32Array>()
                .ok_or("int32")?
                .value(row),
        )),
        DataType::Int64 => Ok(Value::from(
            array
                .as_any()
                .downcast_ref::<arrow::array::Int64Array>()
                .ok_or("int64")?
                .value(row),
        )),
        DataType::UInt8 => Ok(Value::from(
            array
                .as_any()
                .downcast_ref::<arrow::array::UInt8Array>()
                .ok_or("uint8")?
                .value(row),
        )),
        DataType::UInt16 => Ok(Value::from(
            array
                .as_any()
                .downcast_ref::<arrow::array::UInt16Array>()
                .ok_or("uint16")?
                .value(row),
        )),
        DataType::UInt32 => Ok(Value::from(
            array
                .as_any()
                .downcast_ref::<arrow::array::UInt32Array>()
                .ok_or("uint32")?
                .value(row),
        )),
        DataType::UInt64 => Ok(Value::from(
            array
                .as_any()
                .downcast_ref::<arrow::array::UInt64Array>()
                .ok_or("uint64")?
                .value(row) as i64,
        )),
        DataType::Float32 => serde_json::Number::from_f64(
            array
                .as_any()
                .downcast_ref::<arrow::array::Float32Array>()
                .ok_or("float32")?
                .value(row) as f64,
        )
        .map(Value::Number)
        .ok_or_else(|| "float32".into()),
        DataType::Float64 => serde_json::Number::from_f64(
            array
                .as_any()
                .downcast_ref::<arrow::array::Float64Array>()
                .ok_or("float64")?
                .value(row),
        )
        .map(Value::Number)
        .ok_or_else(|| "float64".into()),
        DataType::Boolean => Ok(Value::Bool(
            array
                .as_any()
                .downcast_ref::<arrow::array::BooleanArray>()
                .ok_or("bool")?
                .value(row),
        )),
        DataType::Timestamp(unit, _) => {
            let raw = timestamp_value(array, *unit, row)?;
            let secs = timestamp_secs(raw, *unit);
            let dt = chrono::DateTime::from_timestamp(secs, 0)
                .ok_or_else(|| "timestamp out of range".to_string())?;
            Ok(Value::String(dt.to_rfc3339()))
        }
        other => Err(format!(
            "stream processor cannot read {} values",
            format_data_type(other)
        )),
    }
}

fn timestamp_value(array: &dyn Array, unit: TimeUnit, row: usize) -> Result<i64, String> {
    match unit {
        TimeUnit::Second => Ok(array
            .as_any()
            .downcast_ref::<arrow::array::TimestampSecondArray>()
            .ok_or("timestamp[s]")?
            .value(row)),
        TimeUnit::Millisecond => Ok(array
            .as_any()
            .downcast_ref::<arrow::array::TimestampMillisecondArray>()
            .ok_or("timestamp[ms]")?
            .value(row)),
        TimeUnit::Microsecond => Ok(array
            .as_any()
            .downcast_ref::<arrow::array::TimestampMicrosecondArray>()
            .ok_or("timestamp[us]")?
            .value(row)),
        TimeUnit::Nanosecond => Ok(array
            .as_any()
            .downcast_ref::<arrow::array::TimestampNanosecondArray>()
            .ok_or("timestamp[ns]")?
            .value(row)),
    }
}

fn timestamp_secs(raw: i64, unit: TimeUnit) -> i64 {
    match unit {
        TimeUnit::Second => raw,
        TimeUnit::Millisecond => raw.div_euclid(1_000),
        TimeUnit::Microsecond => raw.div_euclid(1_000_000),
        TimeUnit::Nanosecond => raw.div_euclid(1_000_000_000),
    }
}

fn format_data_type(dt: &DataType) -> String {
    format!("{dt:?}")
}

fn already_applied_slice(
    cursor: &WalCursor,
    segment_id: &str,
    slice_ordinal: u32,
    batch_rows: usize,
) -> bool {
    cursor.segment_id == segment_id
        && cursor.slice_ordinal == slice_ordinal
        && cursor.row_index as usize >= batch_rows
}

pub fn start_processor_service(config: &Config, offsets: Arc<crate::helpers::offsets::Offsets>) {
    let pipeline = config.get_pipeline_name();
    {
        let mut started = PROCESSOR_STARTED.lock().unwrap_or_else(|e| e.into_inner());
        if started.get(&pipeline).copied().unwrap_or(false) {
            return;
        }
        started.insert(pipeline.clone(), true);
    }
    PROCESSOR_STOP.store(false, Ordering::Relaxed);
    let config = config.clone();
    let pipeline_key = pipeline.clone();
    let handle = tokio::spawn(async move {
        if let Err(err) = run_processor_loop(config, offsets).await {
            error!("stream processor stopped: {err}");
        }
        if let Ok(mut started) = PROCESSOR_STARTED.lock() {
            started.insert(pipeline_key, false);
        }
    });
    if let Ok(mut guard) = PROCESSOR_HANDLE.lock() {
        guard.insert(pipeline, handle);
    }
}

async fn run_processor_loop(
    config: Config,
    offsets: Arc<crate::helpers::offsets::Offsets>,
) -> Result<(), String> {
    let store = offsets.open_stream_state()?;
    let fence = offsets.writer_fence().unwrap_or_else(CatalogFence::local);
    let metadata = config.get_metadata().await.map_err(|err| err.to_string())?;
    if config.transform_compute_configured() {
        crate::compute::admit_config(&config, store.as_ref(), fence, &metadata)
            .map_err(|err| err.to_string())?;
    }
    let key =
        crate::compute::journal_key(&config.get_workspace_name(), &config.get_pipeline_name())
            .map_err(|err| err.to_string())?;
    let log = crate::compute::load_journal(store.as_ref(), &key).map_err(|err| err.to_string())?;
    let kinds: Vec<ComputeKind> = log
        .kinds()
        .into_iter()
        .filter(|k| k.is_store_table())
        .collect();
    if kinds.is_empty() {
        reset_reclaim_hold();
        return Ok(());
    }
    let state = StreamState::open(store, fence, config.get_pipeline_name(), kinds)?;
    info!(
        pipeline = %config.get_pipeline_name(),
        kinds = state.kinds.len(),
        "stream processor started"
    );
    loop {
        let processed = process_available(&config, &state).await?;
        if PROCESSOR_STOP.load(Ordering::Relaxed) {
            while process_available(&config, &state).await? > 0 {}
            break;
        }
        if processed == 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    Ok(())
}

async fn process_available(config: &Config, state: &StreamState) -> Result<usize, String> {
    let Some(handle) = crate::buffer::wal_store::ingest_wal_handle() else {
        return Ok(0);
    };
    let mut rows = handle.live_segment_rows().map_err(|err| err.to_string())?;
    rows.sort_by_key(|row| row.seq.get());
    let max_seq = rows.last().map(|row| row.seq.get()).unwrap_or(0);
    let mut processed = 0usize;
    for agg in &state.kinds {
        let Some(id) = agg.store_id() else {
            continue;
        };
        for seg in &rows {
            let meta = state.load_meta(id)?.unwrap_or_else(|| ProcessorMeta {
                cursor: WalCursor::default(),
            });
            let lag = max_seq.saturating_sub(meta.cursor.seq);
            crate::metrics::counters::set_stream_processor_wal_lag(lag);
            processed += process_segment(state, agg, &meta.cursor, seg).await?;
        }
    }
    let _ = config;
    Ok(processed)
}

async fn process_segment(
    state: &StreamState,
    agg: &ComputeKind,
    cursor: &WalCursor,
    seg: &SegmentRow,
) -> Result<usize, String> {
    if !cursor.is_before(seg.seq.get(), seg.segment_id.as_str(), u32::MAX, u64::MAX)
        && cursor.seq > seg.seq.get()
    {
        return Ok(0);
    }
    if cursor.seq > seg.seq.get() {
        return Ok(0);
    }
    let bytes = crate::buffer::wal_handle::payload_bytes(&seg.location, &seg.payload_sha256)
        .map_err(|err| err.to_string())?;
    let meta = SegmentFile::read_metadata_from_bytes(&bytes).map_err(|err| err.to_string())?;
    let mut processed = 0usize;
    let mut finished_last = false;
    let mut current = if cursor.segment_id == seg.segment_id.as_str() {
        cursor.clone()
    } else if cursor.seq < seg.seq.get() {
        WalCursor {
            seq: seg.seq.get(),
            segment_id: seg.segment_id.as_str().to_string(),
            slice_ordinal: 0,
            row_index: 0,
        }
    } else {
        cursor.clone()
    };
    for entry in &meta.index {
        if entry.slice_ordinal < current.slice_ordinal {
            continue;
        }
        if entry.codec != crate::buffer::segment_file::PART_CODEC_ARROW {
            continue;
        }
        let start = entry.start as usize;
        let end = start.saturating_add(entry.len as usize).min(bytes.len());
        let mut cursor_io = std::io::Cursor::new(&bytes[start..end]);
        let batches: Vec<RecordBatch> =
            arrow::ipc::reader::StreamReader::try_new(&mut cursor_io, None)
                .map_err(|err| err.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| err.to_string())?;
        if batches.is_empty() {
            continue;
        }
        let schema = batches[0].schema();
        let batch = if batches.len() == 1 {
            batches.into_iter().next().unwrap()
        } else {
            arrow::compute::concat_batches(&schema, &batches).map_err(|err| err.to_string())?
        };
        let cdc = SegmentFile::read_part_meta_from_bytes(&bytes, entry)
            .map_err(|err| err.to_string())?
            .map(|part| part.rows);
        let start_row = if current.segment_id == seg.segment_id.as_str()
            && current.slice_ordinal == entry.slice_ordinal
        {
            current.row_index as usize
        } else {
            0
        };
        if already_applied_slice(
            &current,
            seg.segment_id.as_str(),
            entry.slice_ordinal,
            batch.num_rows(),
        ) || start_row >= batch.num_rows()
        {
            continue;
        }
        current.slice_ordinal = entry.slice_ordinal;
        current.seq = seg.seq.get();
        current.segment_id = seg.segment_id.as_str().to_string();
        current = state.apply_events(agg, current, &batch, cdc.as_deref(), start_row)?;
        processed += 1;
        finished_last = Some(entry.slice_ordinal)
            == meta.index.last().map(|item| item.slice_ordinal)
            && current.row_index >= batch.num_rows() as u64;
    }
    if processed > 0 {
        if finished_last {
            set_min_unconsumed_seq(&state.pipeline, seg.seq.get().saturating_add(1));
        } else {
            set_min_unconsumed_seq(&state.pipeline, seg.seq.get());
        }
        if let Some(handle) = crate::buffer::wal_store::ingest_wal_handle() {
            if let Err(err) = handle
                .reclaim_fully_done_before(min_unconsumed_seq(&state.pipeline))
                .await
            {
                debug!("stream processor delayed reclaim: {err}");
            }
        }
    }
    Ok(processed)
}

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests_chaos;
#[cfg(test)]
mod tests_fuzz;
#[cfg(test)]
mod tests_sim;

pub async fn drain_and_stop_processor() -> bool {
    PROCESSOR_STOP.store(true, Ordering::Relaxed);
    let handles: Vec<tokio::task::JoinHandle<()>> = PROCESSOR_HANDLE
        .lock()
        .ok()
        .map(|mut g| g.drain().map(|(_, h)| h).collect())
        .unwrap_or_default();
    if handles.is_empty() {
        if let Ok(mut started) = PROCESSOR_STARTED.lock() {
            started.clear();
        }
        return true;
    }
    let mut ok = true;
    for handle in handles {
        if let Err(err) = handle.await {
            warn!("stream processor join failed: {err}");
            ok = false;
        }
    }
    if let Ok(mut started) = PROCESSOR_STARTED.lock() {
        started.clear();
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::test_support::{invoice_batch, invoice_kind};
    use super::*;

    fn agg() -> crate::compute::ComputeKind {
        invoice_kind()
    }

    fn state() -> super::test_support::DiskFixture {
        super::test_support::DiskFixture::new()
    }

    fn insert_meta(id: &str) -> WalRowMeta {
        WalRowMeta::new(
            MutationKind::Insert,
            id.as_bytes().to_vec(),
            id.as_bytes().to_vec(),
        )
    }

    #[test]
    fn running_sum_and_count() {
        let state = state();
        let spec = agg();
        let batch = invoice_batch(&["A", "A", "B"], &[100, 50, 10], &["1", "2", "3"]);
        let cdc = vec![insert_meta("1"), insert_meta("2"), insert_meta("3")];
        state
            .apply_events(
                &spec,
                WalCursor {
                    seq: 1,
                    segment_id: "s1".into(),
                    slice_ordinal: 0,
                    row_index: 0,
                },
                &batch,
                Some(&cdc),
                0,
            )
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            150.0
        );
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_count")
                .unwrap(),
            2.0
        );
        assert_eq!(
            state
                .measure("customer_invoice_totals", "B", "invoice_total")
                .unwrap(),
            10.0
        );
    }

    #[test]
    fn cdc_update_applies_delta_not_absolute() {
        let state = state();
        let spec = agg();
        let first = invoice_batch(&["A"], &[100], &["inv"]);
        state
            .apply_events(
                &spec,
                WalCursor::default(),
                &first,
                Some(&[insert_meta("e1")]),
                0,
            )
            .unwrap();
        let second = invoice_batch(&["A"], &[150], &["inv"]);
        let update = WalRowMeta::new(MutationKind::Update, b"e2".to_vec(), b"e2".to_vec())
            .with_before(super::test_support::invoice_before("A", 100, "inv"));
        state
            .apply_events(
                &spec,
                WalCursor {
                    seq: 0,
                    segment_id: String::new(),
                    slice_ordinal: 0,
                    row_index: 1,
                },
                &second,
                Some(&[update]),
                0,
            )
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            150.0
        );
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_count")
                .unwrap(),
            1.0
        );
    }

    #[test]
    fn cdc_delete_subtracts_and_missing_last_row_is_noop() {
        let state = state();
        let spec = agg();
        let first = invoice_batch(&["A"], &[80], &["inv"]);
        state
            .apply_events(
                &spec,
                WalCursor::default(),
                &first,
                Some(&[insert_meta("e1")]),
                0,
            )
            .unwrap();
        let del = invoice_batch(&["A"], &[80], &["inv"]);
        let meta = WalRowMeta::new(MutationKind::Delete, b"e2".to_vec(), b"e2".to_vec());
        state
            .apply_events(
                &spec,
                WalCursor {
                    row_index: 1,
                    ..WalCursor::default()
                },
                &del,
                Some(&[meta]),
                0,
            )
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            0.0
        );
        let missing = WalRowMeta::new(MutationKind::Delete, b"e3".to_vec(), b"e3".to_vec());
        state
            .apply_events(
                &spec,
                WalCursor {
                    row_index: 2,
                    ..WalCursor::default()
                },
                &del,
                Some(&[missing]),
                0,
            )
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            0.0
        );
    }

    #[test]
    fn higher_token_replaces_same_event_id() {
        let state = state();
        let spec = agg();
        let first = invoice_batch(&["A"], &[40], &["inv"]);
        let low = WalRowMeta::new(MutationKind::Insert, b"same".to_vec(), vec![1]);
        state
            .apply_events(&spec, WalCursor::default(), &first, Some(&[low]), 0)
            .unwrap();
        let second = invoice_batch(&["A"], &[90], &["inv"]);
        let high = WalRowMeta::new(MutationKind::Insert, b"same".to_vec(), vec![2]);
        state
            .apply_events(
                &spec,
                WalCursor {
                    row_index: 1,
                    ..WalCursor::default()
                },
                &second,
                Some(&[high]),
                0,
            )
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            90.0
        );
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_count")
                .unwrap(),
            1.0
        );
        let lower = WalRowMeta::new(MutationKind::Insert, b"same".to_vec(), vec![1]);
        let stale = invoice_batch(&["A"], &[5], &["inv"]);
        state
            .apply_events(
                &spec,
                WalCursor {
                    row_index: 2,
                    ..WalCursor::default()
                },
                &stale,
                Some(&[lower]),
                0,
            )
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            90.0
        );
    }

    #[test]
    fn duplicate_event_id_is_ignored() {
        let state = state();
        let spec = agg();
        let batch = invoice_batch(&["A"], &[40], &["inv"]);
        let cdc = vec![insert_meta("same")];
        state
            .apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0)
            .unwrap();
        state
            .apply_events(
                &spec,
                WalCursor {
                    row_index: 1,
                    ..WalCursor::default()
                },
                &batch,
                Some(&cdc),
                0,
            )
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            40.0
        );
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_count")
                .unwrap(),
            1.0
        );
    }

    #[test]
    fn cdc_update_without_before_does_not_insert_or_retract() {
        let state = state();
        let spec = agg();
        let first = invoice_batch(&["A"], &[25], &["inv"]);
        state
            .apply_events(
                &spec,
                WalCursor::default(),
                &first,
                Some(&[insert_meta("e1")]),
                0,
            )
            .unwrap();
        let moved = invoice_batch(&["B"], &[40], &["inv"]);
        let update = WalRowMeta::new(MutationKind::Update, b"e2".to_vec(), b"e2".to_vec());
        state
            .apply_events(
                &spec,
                WalCursor {
                    row_index: 1,
                    ..WalCursor::default()
                },
                &moved,
                Some(&[update]),
                0,
            )
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            25.0
        );
        assert_eq!(
            state
                .measure("customer_invoice_totals", "B", "invoice_total")
                .unwrap(),
            0.0
        );
    }

    #[test]
    fn pk_change_is_delete_plus_insert() {
        let state = state();
        let spec = agg();
        let first = invoice_batch(&["A"], &[25], &["inv"]);
        state
            .apply_events(
                &spec,
                WalCursor::default(),
                &first,
                Some(&[insert_meta("e1")]),
                0,
            )
            .unwrap();
        let moved = invoice_batch(&["B"], &[25], &["inv"]);
        let update = WalRowMeta::new(MutationKind::Update, b"e2".to_vec(), b"e2".to_vec())
            .with_before(crate::plugins::cdc::BeforeImage {
                key: b"A".to_vec(),
                row: serde_json::to_vec(&BTreeMap::from([
                    ("customer_id".to_string(), Value::String("A".into())),
                    ("amount".to_string(), Value::from(25)),
                    ("invoice_id".to_string(), Value::String("inv".into())),
                ]))
                .unwrap(),
            });
        state
            .apply_events(
                &spec,
                WalCursor {
                    row_index: 1,
                    ..WalCursor::default()
                },
                &moved,
                Some(&[update]),
                0,
            )
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            0.0
        );
        assert_eq!(
            state
                .measure("customer_invoice_totals", "B", "invoice_total")
                .unwrap(),
            25.0
        );
    }

    #[test]
    fn pk_change_inside_one_batch_uses_before_image() {
        let state = state();
        let spec = agg();
        let batch = invoice_batch(&["A", "B"], &[25, 25], &["inv", "inv"]);
        let update = WalRowMeta::new(MutationKind::Update, b"e2".to_vec(), b"e2".to_vec())
            .with_before(crate::plugins::cdc::BeforeImage {
                key: b"A".to_vec(),
                row: serde_json::to_vec(&BTreeMap::from([
                    ("customer_id".to_string(), Value::String("A".into())),
                    ("amount".to_string(), Value::from(25)),
                    ("invoice_id".to_string(), Value::String("inv".into())),
                ]))
                .unwrap(),
            });
        state
            .apply_events(
                &spec,
                WalCursor::default(),
                &batch,
                Some(&[insert_meta("e1"), update]),
                0,
            )
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            0.0
        );
        assert_eq!(
            state
                .measure("customer_invoice_totals", "B", "invoice_total")
                .unwrap(),
            25.0
        );
    }

    #[test]
    fn incompatible_ir_wipe_clears_measures() {
        let fixture = super::test_support::DiskFixture::new();
        let spec = fixture.spec.clone();
        let batch = invoice_batch(&["A"], &[9], &["inv"]);
        fixture
            .apply_events(
                &spec,
                WalCursor::default(),
                &batch,
                Some(&[insert_meta("e1")]),
                0,
            )
            .unwrap();
        assert_eq!(
            fixture
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            9.0
        );
        fixture.wipe_compute("customer_invoice_totals");
        assert_eq!(
            fixture
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            0.0
        );
    }

    #[test]
    fn crash_before_txn_leaves_state_unapplied_then_replay_matches() {
        let spec = agg();
        let batch = invoice_batch(&["A"], &[7], &["inv"]);
        let cdc = vec![insert_meta("e1")];
        let fixture = super::test_support::DiskFixture::new();
        set_test_crash(Some(CrashPoint::BeforeTxn));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = fixture.apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0);
        }));
        set_test_crash(None);
        assert!(panicked.is_err());
        let fixture = fixture.reopen();
        assert_eq!(
            fixture
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            0.0
        );
        fixture
            .apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0)
            .unwrap();
        assert_eq!(
            fixture
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            7.0
        );
    }

    #[test]
    fn crash_during_txn_aborts_then_replay_matches() {
        let spec = agg();
        let batch = invoice_batch(&["A"], &[7], &["inv"]);
        let cdc = vec![insert_meta("e1")];
        let fixture = super::test_support::DiskFixture::new();
        set_test_crash(Some(CrashPoint::DuringTxn));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = fixture.apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0);
        }));
        set_test_crash(None);
        assert!(panicked.is_err());
        let fixture = fixture.reopen();
        assert_eq!(
            fixture
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            0.0
        );
        fixture
            .apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0)
            .unwrap();
        assert_eq!(
            fixture
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            7.0
        );
    }

    #[test]
    fn crash_after_txn_replay_is_deduped() {
        let spec = agg();
        let batch = invoice_batch(&["A"], &[7], &["inv"]);
        let cdc = vec![insert_meta("e1")];
        let fixture = super::test_support::DiskFixture::new();
        set_test_crash(Some(CrashPoint::AfterTxn));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = fixture.apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0);
        }));
        set_test_crash(None);
        assert!(panicked.is_err());
        let fixture = fixture.reopen();
        assert_eq!(
            fixture
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            7.0
        );
        fixture
            .apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0)
            .unwrap();
        assert_eq!(
            fixture
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            7.0
        );
    }

    #[test]
    fn insert_batch_does_not_rescan_pending_entities() {
        let state = state();
        let spec = agg();
        let n = 80usize;
        let customers: Vec<String> = (0..n).map(|i| format!("c{i}")).collect();
        let ids: Vec<String> = (0..n).map(|i| format!("inv{i}")).collect();
        let amounts = vec![1i64; n];
        let customer_refs: Vec<&str> = customers.iter().map(String::as_str).collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let batch = invoice_batch(&customer_refs, &amounts, &id_refs);
        let cdc: Vec<_> = ids.iter().map(|id| insert_meta(id)).collect();
        state
            .apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0)
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "c0", "invoice_count")
                .unwrap(),
            1.0
        );
    }

    #[test]
    fn already_applied_slice_is_skip_not_new_work() {
        let cursor = WalCursor {
            seq: 3,
            segment_id: "seg-a".into(),
            slice_ordinal: 1,
            row_index: 10,
        };
        assert!(already_applied_slice(&cursor, "seg-a", 1, 10));
        assert!(!already_applied_slice(&cursor, "seg-a", 1, 11));
        assert!(!already_applied_slice(&cursor, "seg-b", 1, 10));
        assert!(!already_applied_slice(&cursor, "seg-a", 2, 10));
    }

    #[test]
    fn reclaim_hold_blocks_current_seq() {
        reset_reclaim_hold();
        assert!(allows_reclaim("events", 0));
        set_min_unconsumed_seq("events", 4);
        assert!(allows_reclaim("events", 3));
        assert!(!allows_reclaim("events", 4));
        reset_reclaim_hold();
        assert!(allows_reclaim("events", 4));
    }

    #[test]
    fn faster_compute_does_not_raise_global_min_past_a_slower_one() {
        reset_reclaim_hold();
        set_compute_cursor("events", "slow", 10);
        set_compute_cursor("events", "fast", 20);
        assert_eq!(min_unconsumed_seq("events"), 10);
        set_compute_cursor("events", "fast", 30);
        assert_eq!(min_unconsumed_seq("events"), 10);
        reset_reclaim_hold();
        assert_eq!(skippr_cdc::global_min(&[]), None);
        assert!(allows_reclaim("events", 99));
    }

    #[test]
    fn compaction_snapshot_swap_is_atomic() {
        reset_reclaim_hold();
        set_compute_cursor("events", "a", 7);
        let first = load_compaction_snapshot("events");
        assert_eq!(*first, Some(7));
        set_compute_cursor("events", "a", 3);
        let second = load_compaction_snapshot("events");
        assert_eq!(*second, Some(3));
        assert_eq!(*first, Some(7));
        reset_reclaim_hold();
    }
}
