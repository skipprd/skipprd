use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
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

static ACCUMULATOR: Lazy<TimedRwLock<IndexMap<PartitionKey, IngestBufferBatch>>> = Lazy::new(|| {
    TimedRwLock::new("wal_accumulator".to_string(), IndexMap::with_capacity(128))
});
static BYTES: Lazy<TimedRwLock<HashMap<PartitionKey, u64>>> = Lazy::new(|| {
    TimedRwLock::new("wal_bytes".to_string(), HashMap::with_capacity(128))
});
static FIRST_SEEN: Lazy<TimedRwLock<HashMap<PartitionKey, Instant>>> = Lazy::new(|| {
    TimedRwLock::new("wal_first_seen".to_string(), HashMap::with_capacity(128))
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
    let mut acc = ACCUMULATOR.write();
    let mut bytes_map = BYTES.write();
    let mut first_seen = FIRST_SEEN.write();

    for (k, mut v) in map.into_iter() {
        if let Some(existing) = acc.get_mut(&k) {
            // merge records
            existing.records.append(&mut v.records);
            // merge offsets by taking max per OffsetKey
            for (ok, pos) in v.offsets.into_iter() {
                let entry = existing.offsets.entry(ok).or_insert(0);
                if *entry < pos { *entry = pos; }
            }
        } else {
            acc.insert(k.clone(), v);
            first_seen.insert(k.clone(), Instant::now());
        }
        // Approximate bytes as number of records * avg JSON row size proxy (fallback to 0)
        let batch = acc.get(&k).unwrap();
        let added: u64 = batch.records.iter().map(|r| r.record.to_string().len() as u64).sum();
        let entry = bytes_map.entry(k).or_insert(0);
        *entry = entry.saturating_add(added);
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
        let bytes_map = BYTES.read();
        let first_seen = FIRST_SEEN.read();
        let now = Instant::now();
        for (k, b) in bytes_map.iter() {
            let age_ok = match first_seen.get(k) { Some(t0) => now.duration_since(*t0) >= max_delay, None => false };
            if *b >= bytes_per_file || age_ok {
                ready.push(k.clone());
            }
        }
    }

    if ready.is_empty() { return; }

    let log_flush = Config::truth_value(&Config::getenv("LOG_WAL_UPLOADS", "false")) || Config::truth_value(&Config::getenv("LOG_WAL_DEBUG", "false"));
    if log_flush {
        let bytes_map = BYTES.read();
        let total_bytes: u64 = ready.iter().map(|k| *bytes_map.get(k).unwrap_or(&0)).sum();
        println!("WAL accumulator flush: {} partitions, {} bytes", ready.len(), total_bytes);
    }

    // Drain and flush the ready keys in one Buffers call
    let mut to_flush = Buffers::new();
    {
        let mut acc = ACCUMULATOR.write();
        let mut bytes_map = BYTES.write();
        let mut first_seen = FIRST_SEEN.write();
        let mut drain_map: HashMap<PartitionKey, IngestBufferBatch> = HashMap::with_capacity(ready.len());
        for k in ready.into_iter() {
            if let Some(v) = acc.swap_remove(&k) {
                drain_map.insert(k.clone(), v);
            }
            bytes_map.remove(&k);
            first_seen.remove(&k);
        }
        to_flush.write(drain_map);
    }

    if let (Some(offsets), Some(output)) = (OFFSETS_CELL.get(), OUTPUT_CELL.get()) {
        let _ = to_flush.flush(offsets.clone(), output.clone()).await;
    }
}
