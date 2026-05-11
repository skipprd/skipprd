//! Bounded in-memory cache for S3 WAL segment bodies (compaction reads).
//! Bodies are not retained in `SEGMENT_CACHE` after durable S3 commit; this LRU
//! holds fetched bytes between compaction runs. Byte and entry caps are **autotuned**
//! from host memory (no env knobs).

use crate::helpers::s3::get_object_bytes_for_bucket;
use dashmap::DashSet;
use once_cell::sync::Lazy;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

/// Segment IDs currently being compacted — bodies for these keys are never evicted.
pub static S3_BODY_CACHE_PINS: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

const MIN_CACHE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 2 * 1024 * 1024 * 1024;
const MIN_CACHE_ENTRIES: usize = 8;
const MAX_CACHE_ENTRIES: usize = 256;

/// `(max_bytes, max_entries)` derived from the machine; safe to call anytime (LOG_WAL, tests).
pub fn autotuned_caps() -> (usize, usize) {
    autotune_limits()
}

#[cfg(target_os = "linux")]
fn linux_meminfo_kb(label: &str) -> Option<usize> {
    let data = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in data.lines() {
        let line = line.trim_start();
        if line.starts_with(label) {
            return line.split_whitespace().nth(1)?.parse().ok();
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn linux_memory_hint_bytes() -> Option<usize> {
    const KIB: usize = 1024;
    // Values in /proc/meminfo are kiB
    linux_meminfo_kb("MemAvailable:")
        .or_else(|| linux_meminfo_kb("MemFree:"))
        .or_else(|| linux_meminfo_kb("MemTotal:"))
        .map(|kb| kb.saturating_mul(KIB))
}

#[cfg(target_os = "macos")]
fn darwin_total_ram_bytes() -> Option<usize> {
    use std::mem;
    use std::ptr;
    let mut mib: [i32; 2] = [libc::CTL_HW, libc::HW_MEMSIZE];
    let mut out: u64 = 0;
    let mut sz = mem::size_of_val(&out);
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            &mut out as *mut u64 as *mut libc::c_void,
            &mut sz,
            ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(out as usize)
}

fn system_memory_hint_bytes() -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        return linux_memory_hint_bytes();
    }
    #[cfg(target_os = "macos")]
    {
        return darwin_total_ram_bytes();
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

fn autotune_limits() -> (usize, usize) {
    let n = num_cpus::get().max(1);

    let max_bytes = if let Some(hint) = system_memory_hint_bytes() {
        // ~12% of reported RAM / MemAvailable, clamped (single-process LRU, not whole-machine cache)
        hint.saturating_div(100)
            .saturating_mul(12)
            .clamp(MIN_CACHE_BYTES, MAX_CACHE_BYTES)
    } else {
        // No OS hint: scale with CPU count (common CI / embedded layouts)
        (96usize * 1024 * 1024)
            .saturating_add(n.saturating_mul(48 * 1024 * 1024))
            .clamp(MIN_CACHE_BYTES, MAX_CACHE_BYTES)
    };

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
    fn evict_one(&mut self, pins: &DashSet<String>) -> bool {
        let n = self.order.len();
        if n == 0 {
            return false;
        }
        for _ in 0..n {
            let Some(id) = self.order.front().cloned() else {
                return false;
            };
            if pins.contains(&id) {
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

    fn enforce_limits(&mut self, pins: &DashSet<String>) {
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

    fn put(&mut self, id: String, data: Arc<Vec<u8>>, pins: &DashSet<String>) {
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
        let pins = DashSet::new();
        inner.put("a".into(), Arc::new(vec![0u8; 40]), &pins);
        inner.put("b".into(), Arc::new(vec![0u8; 40]), &pins);
        pins.insert("a".into());
        inner.max_bytes = 50;
        inner.enforce_limits(&pins);
        assert!(inner.map.contains_key("a"));
        assert!(!inner.map.contains_key("b"));
        pins.remove("a");
        inner.enforce_limits(&pins);
        assert!(inner.bytes <= 50);
    }

    #[test]
    fn autotune_limits_in_sane_range() {
        let (b, e) = autotune_limits();
        assert!(b >= MIN_CACHE_BYTES && b <= MAX_CACHE_BYTES);
        assert!(e >= MIN_CACHE_ENTRIES && e <= MAX_CACHE_ENTRIES);
    }
}
