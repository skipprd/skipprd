use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use once_cell::sync::OnceCell;
use once_cell::sync::Lazy;
use tokio::time::{sleep as tokio_sleep, Duration as TokioDuration};

use crate::buffer::ingest_buffer::{Buffers, IngestBufferBatch};
use crate::helpers::configuration::Config;
use crate::helpers::offsets::Offsets;
use crate::plugins::DataOutputPlugin;

// Partition key: (namespace, partition, time, shard)
pub type PartitionKey = (String, String, Option<i64>, String);

static STARTED: OnceCell<()> = OnceCell::new();
static OFFSETS_CELL: OnceCell<Arc<Offsets>> = OnceCell::new();
static OUTPUT_CELL: OnceCell<Arc<Box<dyn DataOutputPlugin + Send + Sync>>> = OnceCell::new();

static ACCUMULATOR: Lazy<DashMap<PartitionKey, IngestBufferBatch>> = Lazy::new(|| {
    DashMap::with_capacity(128)
});
static BYTES: Lazy<DashMap<PartitionKey, AtomicU64>> = Lazy::new(|| {
    DashMap::with_capacity(128)
});
static FIRST_SEEN: Lazy<DashMap<PartitionKey, Instant>> = Lazy::new(|| {
    DashMap::with_capacity(128)
});
// Per-partition EWMA of bytes per row
// Deprecated: bytes-per-row EWMA was used for byte estimation; we now track actual batch memory

pub async fn ensure_running_async(offsets: Arc<Offsets>, output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) {
    let _ = OFFSETS_CELL.set(offsets);
    let _ = OUTPUT_CELL.set(output);
    if STARTED.set(()).is_ok() {
        tokio::spawn(async move {
            loop {
                flush_ready().await;
                tokio_sleep(TokioDuration::from_millis(100)).await;
            }
        });
    }
}

// Fallback for callers without an async context
pub fn ensure_running(offsets: Arc<Offsets>, output: Arc<Box<dyn DataOutputPlugin + Send + Sync>>) {
    let _ = OFFSETS_CELL.set(offsets);
    let _ = OUTPUT_CELL.set(output);
    if STARTED.set(()).is_err() {
        return;
    }

    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(async move {
            loop {
                flush_ready().await;
                tokio_sleep(TokioDuration::from_millis(100)).await;
            }
        });
    } else {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("wal-accumulator")
                .build()
                .unwrap();
            rt.block_on(async move {
                loop {
                    flush_ready().await;
                    tokio_sleep(TokioDuration::from_millis(100)).await;
                }
            });
        });
    }
}

pub fn accumulate_map(map: HashMap<PartitionKey, IngestBufferBatch>) {
    // Use short-lived locks per key to reduce contention
    // Also avoid heavy serialization inside locks; approximate bytes by record count * 256
    const DEFAULT_BYTES_PER_ROW: u64 = 256;

    for (k, mut v) in map.into_iter() {
        let mut existed = true;
        use dashmap::mapref::entry::Entry;
        match ACCUMULATOR.entry(k.clone()) {
            Entry::Occupied(mut occ) => {
                let existing = occ.get_mut();
                if let Some(mut vb) = v.record_batches.take() {
                    if let Some(ref mut eb) = existing.record_batches {
                        eb.append(&mut vb);
                    } else {
                        existing.record_batches = Some(vb);
                    }
                }
                for (ok, pos) in v.offsets.into_iter() {
                    let entry = existing.offsets.entry(ok).or_insert(0);
                    if *entry < pos { *entry = pos; }
                }
            }
            Entry::Vacant(vac) => {
                vac.insert(v);
                existed = false;
            }
        }

        if !existed {
            FIRST_SEEN.insert(k.clone(), Instant::now());
            BYTES.entry(k.clone()).or_insert_with(|| AtomicU64::new(0));
        }

        // Estimate added bytes strictly from materialized Arrow RecordBatches
        let mut added_est_bytes: u64 = 0;
        if let Some(acc) = ACCUMULATOR.get(&k) {
            if let Some(ref batches) = acc.record_batches {
                let batch_bytes: u64 = batches
                    .iter()
                    .map(|b| b.get_array_memory_size() as u64)
                    .sum();
                added_est_bytes = added_est_bytes.saturating_add(batch_bytes);
            }
        }
        if added_est_bytes > 0 {
            if let Some(bytes_counter) = BYTES.get(&k) {
                bytes_counter.fetch_add(added_est_bytes, Ordering::Relaxed);
            }
        }
    }
}

async fn flush_ready() {
    let bytes_per_file = Config::get_wal_bytes_per_file().max(64 * 1024); // safety minimum
    let mut max_delay = Duration::from_secs(Config::get_wal_max_delay_seconds());
    if Config::truth_value(&Config::getenv("LOG_WAL_DEBUG", "false")) {
        max_delay = Duration::from_secs(1);
    }

    // Identify keys to flush
    let mut ready: Vec<PartitionKey> = Vec::new();
    {
        let now = Instant::now();
        for item in BYTES.iter() {
            let k = item.key();
            let b = item.value().load(Ordering::Relaxed);
            let age_ok = FIRST_SEEN.get(k).map(|t0| now.duration_since(*t0) >= max_delay).unwrap_or(false);
            if b >= bytes_per_file || age_ok { ready.push(k.clone()); }
        }
    }

    if ready.is_empty() { return; }

    let log_flush = Config::truth_value(&Config::getenv("LOG_WAL_UPLOADS", "false")) || Config::truth_value(&Config::getenv("LOG_WAL_DEBUG", "false"));
    if log_flush || Config::log_wal_enabled() {
        let total_bytes: u64 = ready.iter().map(|k| BYTES.get(k).map(|c| c.load(Ordering::Relaxed)).unwrap_or(0)).sum();
        println!("WAL accumulator flush: {} partitions, {} bytes", ready.len(), total_bytes);
    }

    // Drain and flush the ready keys with fine-grained lock scopes
    let mut to_flush = Buffers::new();
    let mut drain_map: HashMap<PartitionKey, IngestBufferBatch> = HashMap::with_capacity(128);
    for k in ready.into_iter() {
        if let Some((_, v)) = ACCUMULATOR.remove(&k) {
            drain_map.insert(k.clone(), v);
        }
        BYTES.remove(&k);
        FIRST_SEEN.remove(&k);
    }
    if Config::log_wal_enabled() {
        println!("WAL accumulator: drained {} partitions for flush", drain_map.len());
    }
    to_flush.write(drain_map);

    if let (Some(offsets), Some(output)) = (OFFSETS_CELL.get(), OUTPUT_CELL.get()) {
        if Config::log_wal_enabled() { println!("WAL accumulator: invoking Buffers::flush"); }
        let _ = to_flush.flush(offsets.clone(), output.clone()).await;
    }
}

/// Force-flush ALL accumulated batches regardless of thresholds.
/// Used at end-of-ingest to ensure no in-memory data is left un-WALed.
pub async fn flush_all_now() {
    // Snapshot and drain all keys
    let mut to_flush = Buffers::new();
    let mut drain_map: HashMap<PartitionKey, IngestBufferBatch> = HashMap::with_capacity(ACCUMULATOR.len());
    // Collect keys, then remove to obtain owned values (avoids Clone)
    let keys: Vec<PartitionKey> = ACCUMULATOR.iter().map(|e| e.key().clone()).collect();
    for k in keys.into_iter() {
        if let Some((_, v)) = ACCUMULATOR.remove(&k) {
            drain_map.insert(k.clone(), v);
        }
    }
    BYTES.clear();
    FIRST_SEEN.clear();
    if Config::log_wal_enabled() {
        println!("WAL accumulator: force-flush {} partitions", drain_map.len());
    }
    to_flush.write(drain_map);
    if let (Some(offsets), Some(output)) = (OFFSETS_CELL.get(), OUTPUT_CELL.get()) {
        let _ = to_flush.flush(offsets.clone(), output.clone()).await;
    }
}