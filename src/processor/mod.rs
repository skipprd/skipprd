//! Sled-backed keyed aggregates over committed ingestion WAL data.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arrow::array::{Array, RecordBatch};
use arrow::datatypes::DataType;
use serde_derive::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, error, info, warn};

use crate::buffer::segment_file::SegmentFile;
use crate::helpers::configuration::{AggregateMeasure, Config, TransformAggregate};
use crate::plugins::cdc::{MutationKind, WalRowMeta};
use skippr_wal_catalog::SegmentRow;

pub const STREAM_STATE_TREE: &str = "stream_state";

/// Next unconsumed catalog seq. `u64::MAX` means no processor is holding reclaim.
static MIN_UNCONSUMED_SEQ: AtomicU64 = AtomicU64::new(u64::MAX);
static PROCESSOR_STARTED: AtomicBool = AtomicBool::new(false);
static PROCESSOR_STOP: AtomicBool = AtomicBool::new(false);
static PROCESSOR_HANDLE: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    BeforeTxn,
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
    cursor: WalCursor,
    config_hash: String,
}

pub fn min_unconsumed_seq() -> u64 {
    MIN_UNCONSUMED_SEQ.load(Ordering::Relaxed)
}

pub fn allows_reclaim(segment_seq: u64) -> bool {
    segment_seq < min_unconsumed_seq()
}

pub fn set_min_unconsumed_seq(seq: u64) {
    MIN_UNCONSUMED_SEQ.store(seq, Ordering::Relaxed);
}

pub fn reset_reclaim_hold() {
    MIN_UNCONSUMED_SEQ.store(u64::MAX, Ordering::Relaxed);
}

pub struct StreamState {
    tree: sled::Tree,
    pipeline: String,
    aggregates: Vec<TransformAggregate>,
    config_hash: String,
}

impl StreamState {
    pub fn open(
        tree: sled::Tree,
        pipeline: impl Into<String>,
        aggregates: Vec<TransformAggregate>,
    ) -> Result<Self, String> {
        let pipeline = pipeline.into();
        let config_hash = Config::aggregates_config_hash(&aggregates);
        let state = Self {
            tree,
            pipeline,
            aggregates,
            config_hash: config_hash.clone(),
        };
        state.ensure_config(&config_hash)?;
        Ok(state)
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

    fn entity_key_bytes(&self, id: &str, entity: &str) -> Vec<u8> {
        format!("ent/{}/{id}/{entity}", self.pipeline).into_bytes()
    }

    fn dedup_key(&self, id: &str, event_id: &[u8]) -> Vec<u8> {
        format!("dedup/{}/{id}/{}", self.pipeline, hex::encode(event_id)).into_bytes()
    }

    fn prefix(&self, kind: &str, id: &str) -> Vec<u8> {
        format!("{kind}/{}/{id}/", self.pipeline).into_bytes()
    }

    fn ensure_config(&self, hash: &str) -> Result<(), String> {
        for agg in &self.aggregates {
            let key = self.meta_key(&agg.id);
            match self.tree.get(&key).map_err(|err| err.to_string())? {
                Some(bytes) => {
                    let meta: ProcessorMeta =
                        serde_json::from_slice(&bytes).map_err(|err| err.to_string())?;
                    if meta.config_hash != hash {
                        self.rebuild(&agg.id, hash)?;
                    }
                }
                None => {
                    self.write_meta(
                        &agg.id,
                        &ProcessorMeta {
                            cursor: WalCursor::default(),
                            config_hash: hash.to_string(),
                        },
                    )?;
                }
            }
        }
        if let Some(min_seq) = self
            .aggregates
            .iter()
            .filter_map(|agg| {
                self.load_meta(&agg.id)
                    .ok()
                    .flatten()
                    .map(|meta| meta.cursor.seq)
            })
            .min()
        {
            set_min_unconsumed_seq(min_seq);
        }
        Ok(())
    }

    fn rebuild(&self, id: &str, hash: &str) -> Result<(), String> {
        info!(
            aggregate = id,
            "stream processor rebuilding state after config hash change"
        );
        for prefix in [
            self.prefix("agg", id),
            self.prefix("row", id),
            self.prefix("dedup", id),
            self.prefix("ent", id),
        ] {
            let keys: Vec<_> = self
                .tree
                .scan_prefix(&prefix)
                .filter_map(|item| item.ok().map(|(k, _)| k))
                .collect();
            for key in keys {
                self.tree.remove(key).map_err(|err| err.to_string())?;
            }
        }
        self.write_meta(
            id,
            &ProcessorMeta {
                cursor: WalCursor::default(),
                config_hash: hash.to_string(),
            },
        )?;
        set_min_unconsumed_seq(0);
        Ok(())
    }

    fn write_meta(&self, id: &str, meta: &ProcessorMeta) -> Result<(), String> {
        let bytes = serde_json::to_vec(meta).map_err(|err| err.to_string())?;
        self.tree
            .insert(self.meta_key(id), bytes)
            .map_err(|err| err.to_string())?;
        Ok(())
    }

    pub(crate) fn load_meta(&self, id: &str) -> Result<Option<ProcessorMeta>, String> {
        let bytes = self
            .tree
            .get(self.meta_key(id))
            .map_err(|err| err.to_string())?;
        bytes
            .map(|b| serde_json::from_slice(&b).map_err(|err| err.to_string()))
            .transpose()
    }

    pub fn measure(&self, id: &str, key: &str, measure: &str) -> Result<f64, String> {
        let bytes = self
            .tree
            .get(self.agg_key(id, key))
            .map_err(|err| err.to_string())?;
        let Some(bytes) = bytes else {
            return Ok(0.0);
        };
        let map: BTreeMap<String, f64> =
            serde_json::from_slice(&bytes).map_err(|err| err.to_string())?;
        Ok(*map.get(measure).unwrap_or(&0.0))
    }

    pub fn apply_events(
        &self,
        agg: &TransformAggregate,
        cursor: WalCursor,
        batch: &RecordBatch,
        cdc_rows: Option<&[WalRowMeta]>,
        start_row: usize,
    ) -> Result<WalCursor, String> {
        if start_row >= batch.num_rows() {
            return Ok(cursor);
        }
        maybe_crash(CrashPoint::BeforeTxn);
        let mut measures: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
        let mut last_rows: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
        let mut dedup: Vec<Vec<u8>> = Vec::new();
        let mut next_cursor = cursor.clone();

        for row in start_row..batch.num_rows() {
            let meta = cdc_rows.and_then(|rows| rows.get(row));
            if let Some(meta) = meta {
                if !meta.event_id.is_empty() {
                    let dkey = self.dedup_key(&agg.id, &meta.event_id);
                    if self
                        .tree
                        .get(&dkey)
                        .map_err(|err| err.to_string())?
                        .is_some()
                        || dedup.iter().any(|k| k == &dkey)
                    {
                        next_cursor.row_index = (row as u64) + 1;
                        continue;
                    }
                    dedup.push(dkey);
                }
            }
            let new_row = row_values(batch, row)?;
            let key = encode_key(&agg.key_fields, &new_row)?;
            let mutation = meta.map(|m| m.mutation).unwrap_or(MutationKind::Insert);
            let old_row = self.load_previous_row(agg, &key, &new_row, &last_rows)?;
            apply_mutation(
                agg,
                mutation,
                &key,
                old_row.as_ref(),
                &new_row,
                &mut measures,
                &mut last_rows,
            )?;
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

        let meta = ProcessorMeta {
            cursor: next_cursor.clone(),
            config_hash: self.config_hash.clone(),
        };
        let meta_bytes = serde_json::to_vec(&meta).map_err(|err| err.to_string())?;
        let meta_key = self.meta_key(&agg.id);
        let mut puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        puts.push((meta_key, meta_bytes));
        for (key, values) in &measures {
            let mut current = self.load_measures(&agg.id, key)?;
            for (name, delta) in values {
                *current.entry(name.clone()).or_insert(0.0) += *delta;
            }
            puts.push((
                self.agg_key(&agg.id, key),
                serde_json::to_vec(&current).map_err(|err| err.to_string())?,
            ));
        }
        for (key, row) in &last_rows {
            puts.push((
                self.row_key(&agg.id, key),
                serde_json::to_vec(row).map_err(|err| err.to_string())?,
            ));
            if let Some(entity) = entity_identity(agg, row) {
                puts.push((
                    self.entity_key_bytes(&agg.id, &entity),
                    key.as_bytes().to_vec(),
                ));
            }
        }
        for dkey in &dedup {
            puts.push((dkey.clone(), b"1".to_vec()));
        }

        let result: Result<(), sled::transaction::TransactionError<()>> =
            self.tree.transaction(|tx| {
                for (key, value) in &puts {
                    tx.insert(key.as_slice(), value.as_slice())?;
                }
                maybe_crash(CrashPoint::DuringTxn);
                Ok(())
            });
        match result {
            Ok(()) => {}
            Err(err) => {
                crate::metrics::counters::add_stream_processor_txn_failure(1);
                return Err(format!("stream processor transaction failed: {err:?}"));
            }
        }
        maybe_crash(CrashPoint::AfterTxn);
        set_min_unconsumed_seq(next_cursor.seq);
        Ok(next_cursor)
    }

    fn load_measures(&self, id: &str, key: &str) -> Result<BTreeMap<String, f64>, String> {
        let bytes = self
            .tree
            .get(self.agg_key(id, key))
            .map_err(|err| err.to_string())?;
        match bytes {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|err| err.to_string()),
            None => Ok(BTreeMap::new()),
        }
    }

    fn load_previous_row(
        &self,
        agg: &TransformAggregate,
        new_key: &str,
        new_row: &BTreeMap<String, Value>,
        pending: &BTreeMap<String, BTreeMap<String, Value>>,
    ) -> Result<Option<BTreeMap<String, Value>>, String> {
        if let Some(entity) = entity_identity(agg, new_row) {
            if let Some((_, row)) = pending.iter().find(|(_, row)| {
                !row.is_empty() && entity_identity(agg, row).as_deref() == Some(entity.as_str())
            }) {
                return Ok(Some(row.clone()));
            }
            if let Some(bytes) = self
                .tree
                .get(self.entity_key_bytes(&agg.id, &entity))
                .map_err(|err| err.to_string())?
            {
                let old_key = String::from_utf8_lossy(&bytes).into_owned();
                if let Some(row) = pending.get(&old_key).cloned().filter(|row| !row.is_empty()) {
                    return Ok(Some(row));
                }
                if let Some(row) = self.load_last_row(&agg.id, &old_key)? {
                    return Ok(Some(row));
                }
            }
        }
        if let Some(row) = pending.get(new_key).cloned().filter(|row| !row.is_empty()) {
            return Ok(Some(row));
        }
        self.load_last_row(&agg.id, new_key)
    }

    fn load_last_row(
        &self,
        id: &str,
        key: &str,
    ) -> Result<Option<BTreeMap<String, Value>>, String> {
        let bytes = self
            .tree
            .get(self.row_key(id, key))
            .map_err(|err| err.to_string())?;
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
}

fn apply_mutation(
    agg: &TransformAggregate,
    mutation: MutationKind,
    new_key: &str,
    old_row: Option<&BTreeMap<String, Value>>,
    new_row: &BTreeMap<String, Value>,
    measures: &mut BTreeMap<String, BTreeMap<String, f64>>,
    last_rows: &mut BTreeMap<String, BTreeMap<String, Value>>,
) -> Result<(), String> {
    let old_key = old_row
        .map(|row| encode_key(&agg.key_fields, row))
        .transpose()?;
    let pk_changed = old_key.as_deref().is_some_and(|old| old != new_key);

    match mutation {
        MutationKind::Delete => {
            if let Some(old) = old_row {
                add_deltas(
                    agg,
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
            add_deltas(agg, new_key, new_row, 1.0, measures)?;
            last_rows.insert(new_key.to_string(), new_row.clone());
        }
        MutationKind::Update => {
            if pk_changed {
                if let Some(old) = old_row {
                    add_deltas(agg, old_key.as_ref().unwrap(), old, -1.0, measures)?;
                    last_rows.insert(old_key.unwrap(), BTreeMap::new());
                }
                add_deltas(agg, new_key, new_row, 1.0, measures)?;
            } else if let Some(old) = old_row {
                add_measure_delta(agg, new_key, old, new_row, measures)?;
            } else {
                add_deltas(agg, new_key, new_row, 1.0, measures)?;
            }
            last_rows.insert(new_key.to_string(), new_row.clone());
        }
    }
    Ok(())
}

fn add_deltas(
    agg: &TransformAggregate,
    key: &str,
    row: &BTreeMap<String, Value>,
    sign: f64,
    measures: &mut BTreeMap<String, BTreeMap<String, f64>>,
) -> Result<(), String> {
    let slot = measures.entry(key.to_string()).or_default();
    for (name, measure) in &agg.measures {
        *slot.entry(name.clone()).or_insert(0.0) += sign * measure_value(measure, row)?;
    }
    Ok(())
}

fn add_measure_delta(
    agg: &TransformAggregate,
    key: &str,
    old_row: &BTreeMap<String, Value>,
    new_row: &BTreeMap<String, Value>,
    measures: &mut BTreeMap<String, BTreeMap<String, f64>>,
) -> Result<(), String> {
    let slot = measures.entry(key.to_string()).or_default();
    for (name, measure) in &agg.measures {
        let delta = measure_value(measure, new_row)? - measure_value(measure, old_row)?;
        *slot.entry(name.clone()).or_insert(0.0) += delta;
    }
    Ok(())
}

fn measure_value(measure: &AggregateMeasure, row: &BTreeMap<String, Value>) -> Result<f64, String> {
    if measure.count.is_some() {
        return Ok(1.0);
    }
    let Some(column) = measure.sum.as_deref() else {
        return Err("measure has neither sum nor count".into());
    };
    Ok(json_f64(row.get(column).unwrap_or(&Value::Null)))
}

fn json_f64(value: &Value) -> f64 {
    match value {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Bool(true) => 1.0,
        _ => 0.0,
    }
}

fn encode_key(fields: &[String], row: &BTreeMap<String, Value>) -> Result<String, String> {
    let mut parts = Vec::with_capacity(fields.len());
    for field in fields {
        parts.push(json_key_part(row.get(field).unwrap_or(&Value::Null)));
    }
    Ok(parts.join("\u{1f}"))
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

/// Identity of one business entity using leftover columns (not key or measure sources).
/// Lets a CDC update find the previous row after a key-field change.
fn entity_identity(agg: &TransformAggregate, row: &BTreeMap<String, Value>) -> Option<String> {
    if row.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for (name, value) in row {
        if agg.key_fields.iter().any(|field| field == name) {
            continue;
        }
        if agg
            .measures
            .values()
            .any(|measure| measure.sum.as_deref() == Some(name.as_str()))
        {
            continue;
        }
        parts.push(format!("{name}={}", json_key_part(value)));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\u{1f}"))
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
        other => Err(format!(
            "stream processor cannot read {} values",
            format_data_type(other)
        )),
    }
}

fn format_data_type(dt: &DataType) -> String {
    format!("{dt:?}")
}

pub fn start_processor_service(config: &Config, offsets: Arc<crate::helpers::offsets::Offsets>) {
    if !config.transform_aggregates_configured() {
        reset_reclaim_hold();
        return;
    }
    if PROCESSOR_STARTED
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    PROCESSOR_STOP.store(false, Ordering::Relaxed);
    let config = config.clone();
    let handle = tokio::spawn(async move {
        if let Err(err) = run_processor_loop(config, offsets).await {
            error!("stream processor stopped: {err}");
        }
        PROCESSOR_STARTED.store(false, Ordering::Relaxed);
    });
    if let Ok(mut guard) = PROCESSOR_HANDLE.lock() {
        *guard = Some(handle);
    }
}

async fn run_processor_loop(
    config: Config,
    offsets: Arc<crate::helpers::offsets::Offsets>,
) -> Result<(), String> {
    let tree = offsets.open_sled_stream_state()?;
    let aggregates = config.get_transform_aggregates();
    let state = StreamState::open(tree, config.get_pipeline_name(), aggregates)?;
    info!(
        pipeline = %config.get_pipeline_name(),
        aggregates = state.aggregates.len(),
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
    for agg in &state.aggregates {
        for seg in &rows {
            let meta = state.load_meta(&agg.id)?.unwrap_or_else(|| ProcessorMeta {
                cursor: WalCursor::default(),
                config_hash: state.config_hash.clone(),
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
    agg: &TransformAggregate,
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
            set_min_unconsumed_seq(seg.seq.get().saturating_add(1));
        } else {
            set_min_unconsumed_seq(seg.seq.get());
        }
        if let Some(handle) = crate::buffer::wal_store::ingest_wal_handle() {
            if let Err(err) = handle.reclaim_fully_done_before(min_unconsumed_seq()).await {
                debug!("stream processor delayed reclaim: {err}");
            }
        }
    }
    Ok(processed)
}

pub async fn drain_and_stop_processor() -> bool {
    if !PROCESSOR_STARTED.load(Ordering::Relaxed) {
        return true;
    }
    PROCESSOR_STOP.store(true, Ordering::Relaxed);
    let handle = PROCESSOR_HANDLE.lock().ok().and_then(|mut g| g.take());
    if let Some(handle) = handle {
        if let Err(err) = handle.await {
            warn!("stream processor join failed: {err}");
            return false;
        }
    }
    PROCESSOR_STARTED.store(false, Ordering::Relaxed);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, StringArray};
    use arrow_schema::{Field, Schema};
    use std::sync::Arc;

    fn invoice_batch(customers: &[&str], amounts: &[i64], ids: &[&str]) -> RecordBatch {
        let schema = Schema::new(vec![
            Field::new("customer_id", DataType::Utf8, false),
            Field::new("amount", DataType::Int64, false),
            Field::new("invoice_id", DataType::Utf8, false),
        ]);
        RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(StringArray::from(customers.to_vec())),
                Arc::new(Int64Array::from(amounts.to_vec())),
                Arc::new(StringArray::from(ids.to_vec())),
            ],
        )
        .unwrap()
    }

    fn agg() -> TransformAggregate {
        TransformAggregate {
            id: "customer_invoice_totals".into(),
            key_fields: vec!["customer_id".into()],
            measures: [
                (
                    "invoice_total".into(),
                    AggregateMeasure {
                        sum: Some("amount".into()),
                        ..Default::default()
                    },
                ),
                (
                    "invoice_count".into(),
                    AggregateMeasure {
                        count: Some("*".into()),
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
        }
    }

    fn state() -> StreamState {
        let db = sled::Config::new().temporary(true).open().unwrap();
        let tree = db.open_tree(STREAM_STATE_TREE).unwrap();
        StreamState::open(tree, "events", vec![agg()]).unwrap()
    }

    fn insert_meta(id: &str) -> WalRowMeta {
        WalRowMeta {
            mutation: MutationKind::Insert,
            event_id: id.as_bytes().to_vec(),
            order_token: id.as_bytes().to_vec(),
        }
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
        let update = WalRowMeta {
            mutation: MutationKind::Update,
            event_id: b"e2".to_vec(),
            order_token: b"e2".to_vec(),
        };
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
        let meta = WalRowMeta {
            mutation: MutationKind::Delete,
            event_id: b"e2".to_vec(),
            order_token: b"e2".to_vec(),
        };
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
        let missing = WalRowMeta {
            mutation: MutationKind::Delete,
            event_id: b"e3".to_vec(),
            order_token: b"e3".to_vec(),
        };
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
    fn missing_last_row_on_update_is_treated_as_insert() {
        let state = state();
        let spec = agg();
        let batch = invoice_batch(&["A"], &[40], &["inv"]);
        let update = WalRowMeta {
            mutation: MutationKind::Update,
            event_id: b"e1".to_vec(),
            order_token: b"e1".to_vec(),
        };
        state
            .apply_events(&spec, WalCursor::default(), &batch, Some(&[update]), 0)
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
        let update = WalRowMeta {
            mutation: MutationKind::Update,
            event_id: b"e2".to_vec(),
            order_token: b"e2".to_vec(),
        };
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
    fn config_hash_rebuilds_state() {
        let db = sled::Config::new().temporary(true).open().unwrap();
        let tree = db.open_tree(STREAM_STATE_TREE).unwrap();
        let spec = agg();
        let state = StreamState::open(tree.clone(), "events", vec![spec.clone()]).unwrap();
        let batch = invoice_batch(&["A"], &[9], &["inv"]);
        state
            .apply_events(
                &spec,
                WalCursor::default(),
                &batch,
                Some(&[insert_meta("e1")]),
                0,
            )
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            9.0
        );
        let mut rebuilt = spec.clone();
        rebuilt.measures.insert(
            "other".into(),
            AggregateMeasure {
                count: Some("*".into()),
                ..Default::default()
            },
        );
        let state2 = StreamState::open(tree, "events", vec![rebuilt]).unwrap();
        assert_eq!(
            state2
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
        let db = sled::Config::new().temporary(true).open().unwrap();
        let tree = db.open_tree(STREAM_STATE_TREE).unwrap();
        let state = StreamState::open(tree.clone(), "events", vec![spec.clone()]).unwrap();
        set_test_crash(Some(CrashPoint::BeforeTxn));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = state.apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0);
        }));
        set_test_crash(None);
        assert!(panicked.is_err());
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            0.0
        );
        state
            .apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0)
            .unwrap();
        assert_eq!(
            state
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
        let db = sled::Config::new().temporary(true).open().unwrap();
        let tree = db.open_tree(STREAM_STATE_TREE).unwrap();
        let state = StreamState::open(tree.clone(), "events", vec![spec.clone()]).unwrap();
        set_test_crash(Some(CrashPoint::DuringTxn));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = state.apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0);
        }));
        set_test_crash(None);
        assert!(panicked.is_err());
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            0.0
        );
        state
            .apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0)
            .unwrap();
        assert_eq!(
            state
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
        let db = sled::Config::new().temporary(true).open().unwrap();
        let tree = db.open_tree(STREAM_STATE_TREE).unwrap();
        let state = StreamState::open(tree.clone(), "events", vec![spec.clone()]).unwrap();
        set_test_crash(Some(CrashPoint::AfterTxn));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = state.apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0);
        }));
        set_test_crash(None);
        assert!(panicked.is_err());
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            7.0
        );
        state
            .apply_events(&spec, WalCursor::default(), &batch, Some(&cdc), 0)
            .unwrap();
        assert_eq!(
            state
                .measure("customer_invoice_totals", "A", "invoice_total")
                .unwrap(),
            7.0
        );
    }

    #[test]
    fn reclaim_hold_blocks_current_seq() {
        reset_reclaim_hold();
        assert!(allows_reclaim(0));
        set_min_unconsumed_seq(4);
        assert!(allows_reclaim(3));
        assert!(!allows_reclaim(4));
        reset_reclaim_hold();
        assert!(allows_reclaim(4));
    }
}
