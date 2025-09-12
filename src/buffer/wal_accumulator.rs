use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use dashmap::DashMap;
use once_cell::sync::OnceCell;
use once_cell::sync::Lazy;
use tokio::time::{sleep as tokio_sleep, Duration as TokioDuration};

use crate::buffer::ingest_buffer::{Buffers, IngestBufferBatch};
use crate::helpers::configuration::Config;
use crate::helpers::offsets::Offsets;
use crate::helpers::timed_rwlock::TimedRwLock;
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
static BYTES_PER_ROW: Lazy<DashMap<PartitionKey, AtomicU64>> = Lazy::new(|| {
    DashMap::with_capacity(128)
});

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
                existing.records.append(&mut v.records);
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

        // Estimate added bytes based on rows newly added in this call.
        let added_rows: u64 = {
            let mut rows = 0u64;
            // Note: `v` has been moved into ACCUMULATOR. To approximate added rows,
            // read from ACCUMULATOR and sum rows in record_batches if present; otherwise use records len.
            if let Some(acc) = ACCUMULATOR.get(&k) {
                if let Some(ref batches) = acc.record_batches {
                    rows = batches.iter().map(|b| b.num_rows() as u64).sum();
                }
                rows = rows.saturating_add(acc.records.len() as u64);
            }
            rows
        };
        let est_bpr = BYTES_PER_ROW
            .get(&k)
            .map(|v| v.load(Ordering::Relaxed))
            .unwrap_or(DEFAULT_BYTES_PER_ROW);
        let added = est_bpr.saturating_mul(added_rows);
        if let Some(bytes_counter) = BYTES.get(&k) {
            bytes_counter.fetch_add(added, Ordering::Relaxed);
        }
    }
}

async fn flush_ready() {
    let bytes_per_file = Config::get_wal_bytes_per_file();
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
    if log_flush {
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
    to_flush.write(drain_map);

    if let (Some(offsets), Some(output)) = (OFFSETS_CELL.get(), OUTPUT_CELL.get()) {
        let _ = to_flush.flush(offsets.clone(), output.clone()).await;
    }
}

// Public: update EWMA bytes-per-row after WAL serialization (actual sizes known)
pub fn wal_feedback_bytes_per_row(partition_key: &PartitionKey, actual_bytes: u64, actual_rows: u64) {
    if actual_rows == 0 || actual_bytes == 0 { return; }
    let sample = actual_bytes / actual_rows;
    let entry = BYTES_PER_ROW.entry(partition_key.clone()).or_insert_with(|| AtomicU64::new(sample));
    let prev = entry.load(Ordering::Relaxed);
    // alpha=0.2 -> new = 0.8*prev + 0.2*sample
    let new_est = if prev == 0 { sample } else { ((prev.saturating_mul(8)) + (sample.saturating_mul(2))) / 10 };
    entry.store(new_est.max(1), Ordering::Relaxed);
}
