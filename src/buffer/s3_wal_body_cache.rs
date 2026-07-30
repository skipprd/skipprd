//! Bounded in-memory cache for S3 WAL segment bodies (compaction reads).
//! Bodies are not retained in `SEGMENT_CACHE` after durable S3 commit; this LRU
//! holds fetched bytes between compaction runs. Byte cap is autotuned from the same
//! **shared 70% MemAvailable envelope** as S3 WAL ingest admission (`s3_wal_memory_budget`).

use crate::buffer::s3_wal_memory_budget;
use crate::helpers::s3::get_object_bytes_for_bucket;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

/// Ref-counted segment IDs currently being compacted. Bodies for these keys
/// are never evicted until the last active compaction releases its guard.
static S3_BODY_CACHE_PINS: Lazy<DashMap<String, usize>> = Lazy::new(DashMap::new);

pub struct S3BodyPin {
    segment_ids: Vec<String>,
}

impl Drop for S3BodyPin {
    fn drop(&mut self) {
        for segment_id in self.segment_ids.drain(..) {
            match S3_BODY_CACHE_PINS.entry(segment_id) {
                Entry::Occupied(mut entry) if *entry.get() > 1 => {
                    *entry.get_mut() -= 1;
                }
                Entry::Occupied(entry) => {
                    entry.remove();
                }
                Entry::Vacant(_) => {}
            }
        }
    }
}

pub fn pin_segments<I>(segment_ids: I) -> S3BodyPin
where
    I: IntoIterator<Item = String>,
{
    let mut segment_ids = segment_ids.into_iter().collect::<Vec<_>>();
    segment_ids.sort_unstable();
    segment_ids.dedup();
    for segment_id in &segment_ids {
        S3_BODY_CACHE_PINS
            .entry(segment_id.clone())
            .and_modify(|count| *count += 1)
            .or_insert(1);
    }
    S3BodyPin { segment_ids }
}

const MIN_CACHE_ENTRIES: usize = 8;
const MAX_CACHE_ENTRIES: usize = 256;

/// `(max_bytes, max_entries)` derived from the machine; safe to call anytime (LOG_WAL, tests).
pub fn autotuned_caps() -> (usize, usize) {
    autotune_limits()
}

fn autotune_limits() -> (usize, usize) {
    let hint = s3_wal_memory_budget::memory_hint_bytes()
        .unwrap_or(s3_wal_memory_budget::FALLBACK_MEMORY_HINT_BYTES);
    let max_bytes = s3_wal_memory_budget::split_s3_wal_memory_budget(hint).1;

    // Entry budget: ~one slot per ~40 MiB of byte budget, bounded
    let max_entries = (max_bytes / (40 * 1024 * 1024))
        .max(MIN_CACHE_ENTRIES)
        .min(MAX_CACHE_ENTRIES);

    (max_bytes, max_entries)
}

struct Inner {
    /// LRU order: front = oldest (evict first).
    order: VecDeque<String>,
    map: HashMap<String, Arc<Vec<u8>>>,
    bytes: usize,
    max_bytes: usize,
    max_entries: usize,
}

impl Inner {
    fn new_autotuned() -> Self {
        let (max_bytes, max_entries) = autotune_limits();
        Self {
            order: VecDeque::new(),
            map: HashMap::new(),
            bytes: 0,
            max_bytes,
            max_entries,
        }
    }

    fn refresh_caps(&mut self) {
        let (b, e) = autotune_limits();
        self.max_bytes = b;
        self.max_entries = e;
    }

    fn touch(&mut self, id: &str) {
        if let Some(pos) = self.order.iter().position(|x| x == id) {
            self.order.remove(pos);
        }
        self.order.push_back(id.to_string());
    }

    /// Evict one unpinned LRU entry. Returns false if nothing evictable.
    fn evict_one(&mut self, pins: &DashMap<String, usize>) -> bool {
        let n = self.order.len();
        if n == 0 {
            return false;
        }
        for _ in 0..n {
            let Some(id) = self.order.front().cloned() else {
                return false;
            };
            if pins.contains_key(&id) {
                self.order.rotate_left(1);
                continue;
            }
            self.order.pop_front();
            if let Some(v) = self.map.remove(&id) {
                self.bytes = self.bytes.saturating_sub(v.len());
            }
            return true;
        }
        false
    }

    fn enforce_limits(&mut self, pins: &DashMap<String, usize>) {
        loop {
            let over_bytes = self.bytes > self.max_bytes;
            let over_entries = self.map.len() > self.max_entries;
            if !over_bytes && !over_entries {
                break;
            }
            if !self.evict_one(pins) {
                break;
            }
        }
    }

    fn put(&mut self, id: String, data: Arc<Vec<u8>>, pins: &DashMap<String, usize>) {
        let sz = data.len();
        if let Some(old) = self.map.insert(id.clone(), data) {
            self.bytes = self.bytes.saturating_sub(old.len());
        }
        self.bytes = self.bytes.saturating_add(sz);
        self.touch(&id);
        self.enforce_limits(pins);
    }
}

static CACHE: Lazy<Mutex<Inner>> = Lazy::new(|| Mutex::new(Inner::new_autotuned()));

static LRU_BYTES_GAUGE: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));
static LRU_ENTRIES_GAUGE: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));

fn refresh_gauges(inner: &Inner) {
    LRU_BYTES_GAUGE.store(inner.bytes, Ordering::Relaxed);
    LRU_ENTRIES_GAUGE.store(inner.map.len(), Ordering::Relaxed);
}

pub fn lru_byte_count() -> usize {
    LRU_BYTES_GAUGE.load(Ordering::Relaxed)
}

pub fn lru_entry_count() -> usize {
    LRU_ENTRIES_GAUGE.load(Ordering::Relaxed)
}

/// Remove a segment body from the LRU (e.g. segment fully compacted / cache entry dropped).
pub fn remove(segment_id: &str) {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pos) = g.order.iter().position(|x| x == segment_id) {
        g.order.remove(pos);
    }
    if let Some(v) = g.map.remove(segment_id) {
        g.bytes = g.bytes.saturating_sub(v.len());
    }
    refresh_gauges(&g);
}

pub fn clear_for_tests() {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *g = Inner::new_autotuned();
    refresh_gauges(&g);
    S3_BODY_CACHE_PINS.clear();
}

/// Fetch object bytes and merge into LRU under autotuned caps. Caller must pin `segment_id`
/// during the whole compaction if eviction must not drop this body mid-read.
pub async fn get_or_fetch(
    bucket: &str,
    key: &str,
    segment_id: &str,
) -> std::io::Result<Arc<Vec<u8>>> {
    {
        let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        g.refresh_caps();
        if let Some(v) = g.map.get(segment_id) {
            let arc = v.clone();
            g.touch(segment_id);
            g.enforce_limits(&S3_BODY_CACHE_PINS);
            refresh_gauges(&g);
            return Ok(arc);
        }
    }

    let bytes = get_object_bytes_for_bucket(bucket, key).await?;
    let arc = Arc::new(bytes);
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    g.refresh_caps();
    // Re-check: parallel compaction may have populated.
    if let Some(v) = g.map.get(segment_id) {
        let a = v.clone();
        g.touch(segment_id);
        g.enforce_limits(&S3_BODY_CACHE_PINS);
        refresh_gauges(&g);
        return Ok(a);
    }
    g.put(segment_id.to_string(), arc.clone(), &S3_BODY_CACHE_PINS);
    refresh_gauges(&g);
    Ok(arc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evict_respects_pin() {
        let mut inner = Inner {
            order: VecDeque::new(),
            map: HashMap::new(),
            bytes: 0,
            max_bytes: 100,
            max_entries: 10,
        };
        let pins = DashMap::new();
        inner.put("a".into(), Arc::new(vec![0u8; 40]), &pins);
        inner.put("b".into(), Arc::new(vec![0u8; 40]), &pins);
        pins.insert("a".into(), 1);
        inner.max_bytes = 50;
        inner.enforce_limits(&pins);
        assert!(inner.map.contains_key("a"));
        assert!(!inner.map.contains_key("b"));
        pins.remove("a");
        inner.enforce_limits(&pins);
        assert!(inner.bytes <= 50);
    }

    #[test]
    fn pin_guard_is_ref_counted() {
        let first = pin_segments(["shared".to_string()]);
        let second = pin_segments(["shared".to_string(), "shared".to_string()]);
        assert_eq!(*S3_BODY_CACHE_PINS.get("shared").unwrap(), 2);
        drop(first);
        assert_eq!(*S3_BODY_CACHE_PINS.get("shared").unwrap(), 1);
        drop(second);
        assert!(!S3_BODY_CACHE_PINS.contains_key("shared"));
    }

    #[test]
    fn autotune_limits_in_sane_range() {
        let hint = s3_wal_memory_budget::memory_hint_bytes()
            .unwrap_or(s3_wal_memory_budget::FALLBACK_MEMORY_HINT_BYTES);
        let pool = s3_wal_memory_budget::combined_pool_bytes(hint);
        let (b, e) = autotune_limits();
        assert!(b <= pool);
        assert!(e >= MIN_CACHE_ENTRIES && e <= MAX_CACHE_ENTRIES);
    }
}
